# Identity and Payload as Distinct Lifetimes: The txKernel Virtual File System

**A Technical Report**

**Subject:** The design and mechanics of txKernel's VFS — its entity model,
backend boundary, path resolution, mount architecture, and the file-operation
data path.
**Scope:** The inode/dentry/file/superblock model and its operations; how the
same syscall surface drives multiple filesystems; and how each VFS object is
factored along an *identity ⟂ payload* axis with reference types that make the
factoring explicit and enforced.
**Audience:** Kernel engineers familiar with the traditional Unix/Linux VFS
(inodes, dentries, the dcache, `inode_operations`/`file_operations`, `namei`,
mount points) seeking a precise account of txKernel's re-formulation.
**Conventions:** Code listings are *pseudocode* — simplified control flow with
elided error paths and generics — but all named types, fields, and enum
variants are accurate to the source tree. Source locations are cited as
`path:line`.

---

## Abstract

A conventional VFS represents a file with a small set of fused objects — most
centrally `struct inode`, a single allocation with a single reference count that
simultaneously carries the file's identity (its inode number, link count), its
capability (its operations vtable), and its payload (the page-cache mapping).
The fusion is economical until the lifetimes of these roles diverge — and in a
POSIX filesystem they diverge constantly. An unlinked-but-open file must retain
its payload after its last name is gone. A force-unmounted filesystem must
release its backing store while path walks still reference the mount. Both are
managed, in a fused design, by interpreting one overloaded reference count
differently at different times.

txKernel factors every VFS entity along an *identity ⟂ payload* axis. Identity —
what makes a file *this* file — is pinned by `Cap<T>`. Payload — the operational
resources — is pinned by separate operational evidence (`PayloadCap<T>` or typed
contribution pins). Two further reference types complete a four-rung hierarchy:
`Weak<T>` for stale-tolerant hints and `IdentRef<'g, T>` for guard-scoped,
refcount-free observation during traversal. The factoring is applied at three
strengths — co-located (`DEntry`, `OpenFile`), identity-with-routed-payload
(`RNode`), and fully split into two allocations (`MountIdentity` /
`MountPayload`) — each chosen by whether the entity's identity and payload
lifetimes genuinely diverge.

This report develops the model from the entities up. It establishes the
reference hierarchy and its four motivations (unlinked-open files, force-unmount,
refcount-free hot-path lookup, monotone race degradation); the single backend
trait `FsOps` and its synchronous-step execution model; the path walker as a
suspendable step state machine; the fully-split mount architecture and its
force-unmount payoff; and the `open`/`read`/`write` data path. It then drills
into the machinery those sections compress: the page cache and its sharing with
`mmap`; the special-file and synthetic-descriptor families (pipes, ttys,
devices, eventfd/timerfd/signalfd/epoll); and the blocking/readiness substrate
plus mount internals (propagation, namespaces, boot bringup). A complete case
study — opening a tmpfs file, reading it, unlinking it while open, reading
again, and closing — ties the layers together. The central architectural claim,
stated once and demonstrated repeatedly, is: **a file's identity and its payload
are different things with different lifetimes, so the kernel stores and reclaims
them separately, and the reference types name which one you hold.**

---

## Table of Contents

1. Introduction and Central Thesis
2. The Entity Model
3. The Payload/Identity Split
4. The Backend Boundary: `FsOps` and the Step Model
5. Path Resolution
6. Mount: The Fully-Split Entity
7. The File-Operation Data Path
8. The Page Cache and `mmap`
9. Special Files and the Descriptor Zoo
10. Blocking, Readiness, and Mount Internals
11. Case Study: A File's Whole Life
12. Type and Source Reference

---

## 1. Introduction and Central Thesis

A VFS exists so that one syscall surface (`open`, `read`, `write`, `stat`,
`unlink`, …) drives many filesystems. Traditional Unix answers four questions
with four objects: *what is this file?* (`struct inode`), *what is it called and
where does it sit?* (`struct dentry`), *what is this particular open?*
(`struct file`), *what filesystem is mounted here?* (`struct super_block` +
`vfsmount`). Two vtables — `inode_operations`, `file_operations` — make the
arrangement polymorphic. txKernel keeps every one of these boxes.

What txKernel changes is internal to each box. A `struct inode` fuses three
roles — identity (`i_ino`, `i_nlink`), capability (`i_op`), payload
(`i_mapping`) — into one allocation governed by one in-core count (`i_count`).
The thesis of this report is that those roles have *independent lifetimes*, that
a POSIX filesystem's hardest correctness requirements are precisely the cases
where the lifetimes diverge, and that txKernel handles them by *not fusing* —
splitting each entity along an identity ⟂ payload axis and using distinct
reference types for each half.

The two motivating cases recur throughout:

- **Unlink of an open file.** `unlink` removes the last name (`i_nlink → 0`) but
  an open descriptor keeps the file readable until `close`. *Identity nameless,
  payload alive.*
- **Force-unmount of a busy filesystem.** `umount -f` must release the backing
  device while in-flight walkers still reference the mount. *Payload reclaimed,
  identity must persist until the last resolver releases it.*

---

## 2. The Entity Model

txKernel's VFS has three core entities, each zone-allocated and handed back as a
`Cap<T>` — a refcounted handle that pins the entity's identity (analogous to an
in-core `igrab`/`dget` reference, but pinning identity specifically rather than
the whole fused object).

**`RNode`** — the live inode (`vfs/structure.rs:576`):

```rust
pub struct RNode {
    fs_object_id: FsObjectId,                       // the inode number
    meta: InodeMeta,                                // immutable stat() snapshot
    backing: RNodeBacking,                          // *routes* to the payload
    containing_mount: Option<Weak<MountPayload>>,   // weak: which fs this came from
    wait_points: SpinMutex<Option<RNodeWaitPoints>>,// lazy readiness endpoints
}
```

The defining feature is `backing`: the `RNode` allocation holds identity
(`fs_object_id`, `meta`) and a *routing tag*, not the payload itself.

```rust
pub enum RNodeBacking {
    PageBacked { pc: Cap<PageContainer> },   // regular file → page cache (separate alloc)
    Directory,
    Symlink { target: Box<[u8]> },
    StructBacked { payload: StructPayload }, // pipe / tty / socket / device
    Projected { schema: ProjectionSchemaId, key: ProjectionKey }, // procfs/sysfs: no stored bytes
}
```

`InodeMeta` (`vfs/structure.rs:227`) is an *immutable* snapshot (`mode`, `uid`,
`gid`, `size`, times, `nlinks`, `blocks`, `flags`); the file kind is encoded in
`mode`'s `S_IFMT` bits and decoded by `meta.kind()`. Metadata changes go to the
backend and a fresh snapshot is reloaded — the `RNode` is not a mutable
write-back cache.

**`DEntry`** — a name in the tree (`vfs/structure.rs:866`):

```rust
pub struct DEntry {
    name: InlineName,                                   // ≤255B inline component
    parent: Option<Cap<DEntry>>,                        // STRONG: keeps ancestry pathable
    rnode: Cap<RNode>,                                  // STRONG: the inode this names
    mounted: Option<Weak<MountIdentity>>,               // WEAK: mount-point hint
    children: SpinMutex<BTreeMap<InlineName, Weak<DEntry>>>, // WEAK: the dcache
}
```

The asymmetry — parent strong, children weak — breaks the retention cycle: the
cache never keeps a subtree alive on its own (a child whose real users are gone
fails to upgrade and is dropped lazily), while any live dentry remains pathable
to the root.

**`OpenFile`** — one open description (`vfs/structure.rs:1154`):

```rust
pub struct OpenFile {
    backing: OpenFileBacking,          // Rnode{..} for VFS files; also eventfd/timerfd/pidfd/…
    offset: AtomicU64,                 // shared across dup/fork → one file description
    readdir_cursor: AtomicU64,
    flags: OpenFileFlags,              // read/write/append/cloexec/nonblocking/packet
    nonblocking_override: AtomicI8,
    packet_override: AtomicI8,
    opendir_dentry: Option<Cap<DEntry>>,
}
```

`OpenFileBacking::Rnode { rnode: Cap<RNode> }` is the normal case; the other
variants (`Ufd`, `Eventfd`, `Timerfd`, `SignalFd`, `Pidfd`, `Epoll`, `PosixMq`,
`SocketPair`, …) model the non-inode fds that Linux also exposes as files. The
atomic `offset` on the shared `OpenFile` gives the POSIX shared-offset-across-dup
semantic directly.

For an open regular file, two independent reference chains reach one `RNode`:
the **open edge** (`fds[fd] → Cap<OpenFile> → Cap<RNode>`) and the **name edge**
(`parent dir → DEntry → Cap<RNode>`). They are created at different times and
released at different times — the structural fact §3 and §8 exploit.

---

## 3. The Payload/Identity Split

### 3.1 The three-way decomposition

The object model (`object_model_v2.md` §3) decomposes every entity into
**identity** (what makes it this one; pinned by `Cap<T>`), **capability** (the
operations it supports), and **payload** (operational resources; pinned by
`T::OperationalEvidence`). For a regular file: identity is "inode 42 on this
mount" (`RNode`); capability is the `FsOps` vtable plus the `RNodeBacking` tag;
payload is the bytes (a `PageContainer` reached through `backing`).

### 3.2 The reference hierarchy

Four reference types form a ladder (`object_model_v2.md` §4):

```
   Weak<T>                  nullable, epoch-independent, no retention
      │  observe under an epoch guard
      ▼
  IdentRef<'g, T>           epoch-guarded borrow, stack-bound, no retention
      │  pin under the guard (SENTINEL_DEAD CAS)
      ▼
   Cap<T>                   refcounted pin on IDENTITY; 'static
      │  upgrade payload
      ▼
  T::OperationalEvidence    pins PAYLOAD; entails Cap<T>
```

- `Weak<T>` — generation-tagged slot reference, no retention; upgrade fails if
  the slot was reused. *VFS:* `DEntry.children`, `DEntry.mounted`,
  `RNode.containing_mount`.
- `IdentRef<'g, T>` — epoch-guarded pointer, lifetime-bound to the guard,
  cannot escape, no retention. *VFS:* every hop of a path walk.
- `Cap<T>` — refcounted identity pin, `'static`, storable across `.await`.
  *VFS:* `OpenFile`'s and `DEntry`'s `Cap<RNode>`.
- `T::OperationalEvidence` — payload pin; `Cap<T>` for co-located entities,
  `PayloadCap<T>` for indirected payload, typed contribution pins for compound
  predicates.

Holding a higher rung entails every lower one; downgrade is free, upgrade may
fail (the entity died). That asymmetry is the mechanism behind clean race
handling (§3.4).

### 3.3 Four motivations

**Unlinked-open files.** An inode's payload is live under a *disjunction*
(`object_model_v2.md` §3.3): `payload_live ⇔ nlinks > 0 ∨ open_refs > 0`, each
disjunct counted by a separate typed pin (`LinkPin`, `OpenPin`). `unlink` drops
the name edge's `Cap<RNode>` and a `LinkPin`; the open edge's `Cap<RNode>` and
`OpenPin` keep identity and payload alive until `close`. The zombie file is a
representable state, not a refcount accident.

**Force-unmount.** Splitting the superblock into separately-allocated
`MountIdentity` and `MountPayload` (§6) lets the payload (device, `FsOps`)
reclaim at unmount while identity persists for in-flight resolvers, whose
payload upgrade then fails cleanly rather than dereferencing freed memory.

**Refcount-free traversal.** Path resolution and fd lookup run under a single
epoch guard, dereferencing each hop as an `IdentRef` — "no atomic RMW" per hop
(`object_model_v2.md` §2.5) — and upgrading to `Cap` only at the terminal. EBR's
classic stall problem is avoided because guards are lifetime-bound to stack
scope and cannot escape.

**Monotone race degradation.** Liveness predicates are monotone (true→false
once; `object_model_v2.md` §5), so the `IdentRef → Cap` upgrade (a SENTINEL_DEAD
CAS) either succeeds at a point where the entity was live or fails genuinely —
never races a revival. Operations fail to `ENOENT`/`ESTALE` cleanly instead of
pinning a zombie.

### 3.4 Three factoring strengths

The split is a spectrum; the VFS uses three points (`object_model_v2.md` §3.2,
§8.1.1):

| Entity | Factoring | Rationale |
|---|---|---|
| `DEntry`, `OpenFile` | co-located (single slot; `OperationalEvidence = Cap<T>`) | no degraded state — present-or-gone |
| `RNode` | identity slot + payload routed via `backing` | payload shape varies; must outlive names |
| `Mount` | two separate allocations | force-unmount demands payload-dies-identity-lingers |

---

## 4. The Backend Boundary: `FsOps` and the Step Model

A filesystem plugs in by implementing `FsOps` (`vfs/execution.rs:66`),
dispatched as `Arc<dyn FsOps>` — the consolidation of Linux's
`inode_operations` + `file_operations`. Its ~13 core methods
(`lookup`, `load_inode_meta`, `create_inode`, `mkdir`, `symlink`, `unlink`,
`rmdir`, `link`, `rename`, `readdir`, `destroy_inode`, …) all return
`StepOutcome<T, NoProgress>` and operate on `FsObjectId` (inode numbers) and
`InodeMeta` (snapshots) — never on live `RNode`/`DEntry` objects. This is the
**stateless-per-inode** rule (`TX_EXT4_PLAN_v1_2.md`): the backend owns storage;
the VFS owns live identity. Defaulted methods return `ENOSYS`, so a backend
implements only what it supports.

Critically, `unlink` removes one name and decrements the link count but **must
not** destroy the inode (`vfs/execution.rs:96`); reclamation is the separate
`destroy_inode`, invoked by the VFS only when the payload liveness predicate
reaches false. The §3.3 split is enforced as two distinct trait methods.

The byte surface is a companion trait, `FsPageBacking`
(`page_backed/fs_page_backing.rs:34`) — `fetch_page`, `flush_page`, `truncate`,
`fsync_file` — the analogue of `address_space_operations`. A page-cache-backed
filesystem implements both traits; a projection filesystem implements only
`FsOps`. A backend hands both as a `MountOutput { fs_ops, fs_page_backing,
root_fs_object_id, root_inode_meta }`.

**The step model.** A *step* is a synchronous, bounded unit that takes its own
epoch guard and returns one of four outcomes (`tx-substrate/src/step/mod.rs:272`):

```rust
pub enum StepOutcome<T, P> {
    Continue { progress: P },                  // call again
    Yield { progress: P, shape: YieldShape },  // must wait — names the wait source
    Done(T),
    Err(Errno),
}
```

`FsOps` methods use `P = NoProgress` (one-shot identity queries). A *driver*
(`drive`/`drive_oneshot`) loops the step, turning a `Yield` into a real
`.await` suspension in the syscall future and registering the named waker — so a
backend author writes synchronous code returning `StepOutcome`, never touches
`async`, never holds a guard across a suspension, and never blocks a hart. This
is why every backend method takes `&Guard`: it is the step's guard, scoped to
that one synchronous call. (The reactor report details the executor side.)

**The backing gallery** — four filesystems, four payload styles:

| Backend | `RNodeBacking` | Implements | Payload location | `Yield`s? |
|---|---|---|---|---|
| tmpfs | `PageBacked` | `FsOps` + `FsPageBacking` | in-memory `PageContainer` | no |
| procfs | `Projected` | `FsOps` (+ projected read) | synthesised at read | no |
| devfs | `StructBacked` | `FsOps` | device subsystem | device-dependent |
| ext4/FAT | `PageBacked` | both, over `BlockDevice` | on disk via page cache | yes |

tmpfs is the canonical complete backend: state is a `BTreeMap<FsObjectId,
TmpfsInode>` behind a lock (`tmpfs/mod.rs:200`); a regular file's bytes live in
a `Cap<PageContainer>` held by `TmpfsPayload::RegularFile`, not in the inode
record — the split, inside the backend.

---

## 5. Path Resolution

The walker turns a path into a `PathResolution { dentry, rnode, fs_object_id,
meta }` (`resolution/state.rs:112`) by folding over components. Its entire state
is one frame (`resolution/state.rs:74`):

```rust
pub struct WalkingState {
    pub current: Cap<DEntry>,     // lookup cursor
    pub remaining: Vec<u8>,       // unconsumed bytes
    pub hop_count: u32,           // symlink follows (ELOOP at 41)
    pub mount_root: Cap<DEntry>,  // namespace root: bounds `..`, restarts absolute symlinks
    pub must_be_directory: bool,  // trailing-slash flag
}
```

`kernel_step` (`resolution/step.rs:51`) advances one component: terminal check →
`.`/`..` (bounded at `mount_root`) → directory check → POSIX search permission
→ lookup (dentry cache first via `cached_child`, else `FsOps::lookup`) → symlink
splicing → mount crossing. Each stage maps onto traditional `namei`
(`handle_dots`, `inode_permission`, `__d_lookup`/`i_op->lookup`, `step_into`,
`__follow_mount`).

The driver (`resolution/driver.rs:34`) loops `kernel_step` under **one** epoch
guard. `fs_ops_for(&current)` upgrades the `RNode`'s `containing_mount` weak ref
to find the filesystem for each hop (`walker.rs:277`). When a backend `lookup`
returns `Yield`, `kernel_step` returns `NeedIO(IORequest, ResumeToken)` — the
token serialises the entire `WalkingState`, so the walk is a reconstructable
*value*, not a parked stack; the suspendable driver turns it into a `.await` and
`resume_walker` rebuilds the frame when the page arrives. Refcount-free
traversal (§3.3) is concrete here: pointer-chasing under the guard, with `Cap`
upgrades only at the terminal. `step_open` (`walker.rs:202`) composes the walk
with the open-permission check and `OpenFile::new_cap_with_dentry`.

---

## 6. Mount: The Fully-Split Entity

Mount is the maximal split: two separate zone allocations.

```rust
pub struct MountIdentity {                 // placement / identity
    id: MountId,
    mountpoint: Option<Cap<DEntry>>,
    root: Cap<RNode>,
    parent: Option<Cap<MountIdentity>>,
    payload: PayloadBinding<MountPayload>, // detachable link
    flags: AtomicU64, propagation: AtomicU64, peer_group: AtomicU64,
}
pub struct MountPayload {                  // the live filesystem instance
    payload_pin_count: AtomicU32,
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub backing: Option<Arc<dyn BlockDevice>>,
    pub dev_id: DevId, pub options: MountOptions,
    pub fstype: &'static str, pub source_label: SourceLabel,
}
```

(`mount/mod.rs:413`, `:292`.) `MountIdentity` reaches its payload through a
`PayloadBinding`, upgraded by `payload_cap() -> Result<PayloadCap<MountPayload>,
Dead>` (`mount/mod.rs:481`). The walker crosses a mount when a `DEntry`'s
`mounted` weak hint upgrades (§5); the per-process `MountNamespace` holds the
mount table and constrains visibility.

**Force-unmount** is the payoff. Unmount detaches the payload binding:
`MountPayload` — and with it the `FsOps`, `FsPageBacking`, and block device —
reclaims as the last `MountPayloadPin`/`PayloadCap` drops. An in-flight resolver
still holding `Cap<MountIdentity>` keeps identity alive; its next `payload_cap()`
returns `Err(Dead)`, and it unwinds with an error instead of touching freed
memory. Identity dies later, when the last resolver releases it. "Identity
alive, payload `Dead`" is a first-class handled state — the §3.3 cross-axis
independence rule, mechanised by a `Result` type.

The mutating syscalls are composite step ops (`composite.rs`): `MkdirOp`,
`MknodOp`, `UnlinkOp`, `RenameOp`, `SymlinkOp`, `LinkOp`, `ChmodOp`/`ChownOp`,
`TruncateOp`, `StatOp`/`StatxOp`, `ReadLinkOp`, `Getdents64Op` — each a walk
followed by a backend call, holding its resolved target so it can resume after a
walker yield.

---

## 7. The File-Operation Data Path

The fd table is `ProcessPayload.fds: BTreeMap<u32, Cap<OpenFile>>`
(`process/structure.rs:1148`) — in the *payload* half of the process split, so
it is released at exit. Holding `Cap<OpenFile>` makes `dup`/`fork` clone a
shared description (shared atomic offset).

`sys_openat` (`fs_basic.rs:718`): copy path, decode flags, check rlimit, resolve
the start directory (cwd or `dirfd`'s `opendir_dentry`), walk (§5), create on
`O_CREAT`+`ENOENT` via `FsOps::create_inode` then re-walk, truncate on
`O_TRUNC`, build `OpenFile::new_cap_with_dentry`, install at the lowest free fd.

`sys_read` (`io.rs:2179`) resolves the fd, branches on backing kind (non-VFS fds
to their own handlers), and for a VFS file either takes the page-backed fast
path (`sys_read_pagebacked`) or drives `OpenFileReadOp` into a staging buffer
then `bootstrap_copy_to_user`. The dispatch heart is `OpenFile::step_read`
(`vfs/execution.rs:334`), matching on `RNodeBacking`:

| `RNodeBacking` | routes to | gallery backend |
|---|---|---|
| `PageBacked { pc }` | `page_backed::step_read_to_kernel` | tmpfs, ext4/FAT |
| `Projected { .. }` | `fs_ops().step_read_projected` (via `containing_mount` upgrade) | procfs/sysfs |
| `StructBacked { CharDevice }` | `binding.ops.read` | devfs driver |
| `StructBacked { Pipe(Reader) }` | `pipe::step_read` | pipe ring |
| `StructBacked { Tty }` | `tty::execution::step_read` | tty discipline |
| `Directory` | `EISDIR` | — |

One method, one identity object, the `backing` tag steering each read to the
right payload. `sys_write` mirrors this (`step_write`, `vfs/execution.rs:591`)
with `O_APPEND` seek-to-end and writability checks. `step_lseek`
(`vfs/execution.rs:469`) updates the atomic `offset` (`ESPIPE` for non-seekable
backings; directories seek `readdir_cursor`). `sys_close` is `set_fd(fd, None)`,
dropping the `Cap<OpenFile>` and, transitively, possibly the last `Cap<RNode>`.

---

## 8. The Page Cache and `mmap`

A regular file's payload is a `PageContainer` (`page_backed/mod.rs:390`),
reached through `RNodeBacking::PageBacked { pc }` and retained by
`Cap<PageContainer>` independently of the inode's identity — the structural
basis for the unlinked-open file. Its `kind` (`page_backed/mod.rs:279`) decides
where missing pages come from: `Anon` (zeroed frames), `File { mount:
MountPayloadPin, fs_object_id }` (fetched via `FsPageBacking`), or `Device`
(fixed MMIO aperture). Note the `MountPayloadPin`: **the page cache pins the
mount payload it fetches from**, so a filesystem's backing cannot reclaim while
its pages are cached. Inside a `SpinMutex`, state is a sparse `BTreeMap<PageIndex,
PageCacheEntry>` (page → `Ppn` + `PageMarks { dirty, writeback, referenced,
no_reclaim }`) — the analogue of Linux's `i_mapping` xarray and `PG_*` flags.

**Demand paging.** `materialize_page` (`page_backed/mod.rs:759`) dispatches on
`kind` and returns `MaterializedPage { ppn, map_pin, .. }` — the physical page
plus a `MapPin` certifying the VM layer may install it. File pages require
fetch coordination: `begin_file_page_fetch` (`page_backed/mod.rs:916`) elects
exactly one **owner** to call `FsPageBacking::fetch_page` while concurrent
faulters **join** on a shared `PageReadyWait`; the owner installs via
`install_if_absent` (races fall back to the winner) and fires the wait. This is
Linux's `lock_page`/`readpage` serialization re-expressed as explicit
owner/joiner coordination with no lock held across I/O — `fetch_page` returning
`Yield` suspends the future (ext4 disk read), `Done` proceeds (tmpfs).

**The data loop.** `step_read`/`step_write` (`:1239`, `:1265`) clip to file size
and hand to `step_range` (`:1305`), which walks the byte range one page at a
time, accumulating a `ByteProgress` so a partially-complete transfer that yields
mid-range resumes from the saved offset rather than restarting. `step_write`
raises the authoritative file length via `grow_size_to` (`:1223`, a monotone CAS
loop) — file size lives in the `PageContainer`, which is why `load_inode_meta`
reads it back from there. The user/kernel byte copy is a separate explicit step
(`step_read_to_user` / `copy_chunk_user`, `user_buffer.rs`), itself able to yield
if the *user* page is not resident.

**`mmap` shares frames.** A file mapping is a `VmEntry` with `VmEntryBacking::
Page { offset }` (`vm/structure/types.rs:353`) holding a weak ref to the
`PageContainer`. A fault in that range calls the *same* materialization path
(`materialize_page_for_fault_step`, `vm/structure/types.rs:1072`) and installs
the cache's `Ppn` into the page table — so `mmap` and `read` of one file reach
one set of physical frames, exactly as `filemap_fault` populates PTEs from the
same page cache `read` uses. `MAP_PRIVATE` diverges only on first write
(copy-on-write into a private frame; the shared cache page is untouched).

**Writeback.** A write only sets `PageMarks.dirty`. `step_fsync`
(`page_backed/lifecycle.rs:92`) snapshots the dirty set and flushes each page via
`FsPageBacking::flush_page`, counting a `PageProgress` so a large `fsync` resumes
across yields; `clear_dirty_if_match` keeps a page dirty if a write raced the
flush. `truncate` drops or zero-extends pages and updates `size_bytes`.

---

## 9. Special Files and the Descriptor Zoo

The `read`/`write`/`close` surface also drives non-regular files via two
mechanisms. **VFS-rooted special files** have an `RNode` (a path, a `stat`) but
`RNodeBacking::StructBacked { payload: StructPayload }` (`vfs/structure.rs:546`)
routes operations to a kernel object: `Tty`, `CharDevice`, `BlockDevice`,
`Pipe { payload, side }`, `Socket`, namespace files. **Non-VFS descriptors**
have no inode — they are typed `OpenFileBacking` variants (`vfs/structure.rs:
1016`): `Eventfd`, `Timerfd`, `SignalFd`, `Epoll`, `Pidfd`, `PosixMq`, `Ufd`,
`AioContext`, `IoUring`, `SocketPair`, `MountApi`. The uniformity is at the
fd-table layer: every fd is a `Cap<OpenFile>` and every I/O enters
`OpenFile::step_read`/`step_write`, which rejects shapes it can't service
generically and matches `RNodeBacking` for the rest (§7). Where Linux forces a
fake `anon_inode` under each synthetic fd, txKernel reuses the `RNode` path only
for the genuinely-in-tree files and keeps the rest inode-free.

**Pipes** (`pipe/mod.rs:131`): one `PipePayload` (ring + `reader_count`/
`writer_count` + reader/writer `WaitSource`s) shared by reader-end and
writer-end `RNode`s via `PipeSide`. `step_read` (`pipe/mod.rs:893`) states the
semantics — data present → drain + wake writers; empty + no writers → `Done(0)`
(EOF); empty + nonblocking → `EAGAIN`; empty + blocking → yield on the reader
source. `step_write` mirrors it (`EPIPE` → `SIGPIPE` at the syscall arm), with
≤`PIPE_BUF` (4096) writes atomic.

**TTYs**: `StructPayload::Tty(Cap<TtyIdentity>)` routes to `tty::execution`,
which applies the line discipline (canonical vs raw, `VMIN`/`VTIME`), enforces
job control (`SIGTTIN`/`SIGTTOU`), and exposes control through a *typed* ioctl
enum, `OpenFileIoctl` (`vfs/structure.rs:459`): `Tcgets`/`Tcsets`,
`Tiocgpgrp`/`Tiocspgrp`, `Tiocgwinsz`/`Tiocswinsz`, `Tiocsctty`/`Tiocnotty`,
returning `OpenFileIoctlResult` (with a `SideEffect` arm for signal/pgrp
consequences). Non-tty backings get `ENOTTY`.

**Char devices**: `StructPayload::CharDevice(&'static CharDeviceBinding)` with a
`CharDeviceOps { read, write }` trait (`device.rs:47`) and a `DevT` major/minor.
devfs supplies tiny implementations — `/dev/null` (read `Done(0)`, write
discard), `/dev/zero` (read zeros), `/dev/urandom` (read PRNG) — and `step_read`
simply calls `binding.ops.read`. The filesystem owns name/identity; the device
subsystem owns behaviour.

**Synthetic fds**, each a small object with its own semantics and creating
syscall: eventfd (`eventfd/mod.rs:121`, a 64-bit counter; read drains, write
adds, semaphore mode decrements by one), timerfd (`timerfd/mod.rs:363`, expiry
count), signalfd (`signalfd/mod.rs:419`, pending signals as 128-byte records),
epoll (`epoll/mod.rs:59`, a readiness aggregator over other fds' wait-sources),
pidfd (a pollable process handle), socketpair (two `PipePayload`s wired
bidirectionally). The staged subsystems (`Ufd`, `AioContext`, `IoUring`,
`MountApi`) reject generic `read`/`write` with `EINVAL` pending their own arms.

**`fcntl(F_SETFL)`**: `OpenFile` carries `nonblocking_override`/`packet_override`
as `AtomicI8` with a `-1` "unset" sentinel (`vfs/structure.rs:1170`); `flags()`
folds them over the open-time flags so a post-open `O_NONBLOCK` toggle takes
effect on the next I/O without a hot-path lock.

---

## 10. Blocking, Readiness, and Mount Internals

**Wait-sources.** Every "block until ready" ends in a registry-addressable
`WaitSource`: a task parks by subscribing its `TaskMailbox`, a producer fires to
wake all subscribers. VFS files lazily allocate `RNodeWaitPoints` (read/write
`WaitSource` pair, `vfs/notification.rs:30`) — lazy because pipes/ttys/eventfds
own their sources on the backing object, and eager allocation would be unbounded
registry pressure under path-heavy workloads. Readiness is a two-bit mask
(`VFS_READABLE=0x1`, `VFS_WRITABLE=0x2`); `fire_read_wait`/`fire_write_wait`
(`vfs/structure.rs:808`) wake waiters. A blocking read returns `StepOutcome::
Yield { shape: OnWaitSource { source, interests } }`; the driver subscribes the
mailbox and returns `Poll::Pending` — **no kernel stack is parked**, the blocked
read is a suspended future plus a subscription. A producer (`write` into the
ring) fires the source; the future is re-polled and retries.

**`poll`/`epoll`** build on the same sources: `PpollOp` (`vfs/composite.rs:968`)
yields on one fd's source; `epoll_wait` (`linux_syscall/epoll.rs`) parks on every
registered fd's source at once, mapping each backing to a readiness predicate
(`ready_mask_for_entry`). **`EINTR`**: blocking syscalls register on the fd's
source *and* the thread signal mailbox (`await_any_wait_source`, `io.rs:430`); a
signal winning the race yields `EINTR` (or restarts under `SA_RESTART`). This is
Linux's `wait_event_interruptible` + `TIF_SIGPENDING`, made explicit as
dual-subscription on a shared substrate.

**Mount propagation** (`mount/mod.rs:121`): `Propagation { Private, Shared,
Slave, Unbindable }` plus a `peer_group` atomic on the payload control whether
mount/unmount events replicate to peers (`mount --make-shared`). **Bind mounts**
(`bind_mount`, `mount/mod.rs:808`) create a fresh `MountIdentity` cloning the
source's payload — two paths, one filesystem, no new backend. **Remount**
(`mount/mod.rs:974`) is an atomic flag swap (torn-free for walkers).

**Namespaces**: each process resolves through a `MountNamespace` (`mount/mod.rs:
524`) whose table maps `(parent payload, mountpoint inode) → mount`; the walker
consults it to detect mount points, so different namespaces resolve one path
into different filesystems. `CLONE_NEWNS` is `clone_ns` (`:635`, shallow table
copy); the namespace lives in the immutable `NsProxy` bundle (`process/
nsproxy.rs`), and `setns` rebuilds the bundle with a swapped mount namespace.
The **new mount API** models a mount as a configurable file, `MountApiFile`
(`mount/mod.rs:169`): `fsopen`→`FsContext`, `fsmount`→`DetachedMount`,
`open_tree`→subtree snapshot — an alternate front-end to the same
`MountIdentity`/`MountPayload`.

**Boot bringup** resolves the chicken-and-egg of "need a root to mount, mount
through the VFS" by hand-constructing the root mount: `mount_rootfs_tmpfs`
(`init.rs:557`) builds a tmpfs `MountPayload`, root `RNode`(Directory), root
`DEntry`(`/`), and `MountIdentity(MountId(1))`, then publishes a
`MountNamespace`. devfs (`:636`), procfs (`:755`), `/dev/shm`, and an ext4 from
`vda` stack on top via the normal path. An initramfs is unpacked by
`unpack_initramfs` (`initramfs.rs:88`), which parses cpio-newc (magic `070701`,
`TRAILER!!!` sentinel) and replays entries through `kernel_mkdir`/`kernel_create`/
`kernel_symlink` — the direct analogue of Linux `init_mount_tree` +
`populate_rootfs`, building the split-object quartet instead of `vfsmount`/
`super_block`/`dentry`/`inode`.

---

## 11. Case Study: A File's Whole Life

```c
int fd = open("/tmp/foo", O_RDONLY);  // [A]
read(fd, buf, 100);                   // [B]
unlink("/tmp/foo");                   // [C]
read(fd, buf, 100);                   // [D]  ← still works
close(fd);                            // [E]  ← inode finally freed
```

`/tmp` is tmpfs; `foo` is inode `FsObjectId(42)`, 100 bytes in a `PageContainer`.

**[A]** `sys_openat` walks `/tmp` (mount crossing via the `mounted` hint) then
`foo` (tmpfs `lookup` → `Done(42)`, no yield), materialises `RNode(42)` with
`PageBacked { pc }`, builds `DEntry "foo"`, and installs `Cap<OpenFile>` at `fd`.
Two chains now pin `RNode(42)`: the open edge and the name edge.

**[B]** `sys_read` → page-backed lane → `step_read_to_kernel`; 100 bytes copied,
offset → 100. The read reached the bytes via the *open* chain.

**[C]** `UnlinkOp` → `FsOps::unlink(parent, "foo", 42)`: tmpfs removes the
directory entry and sets `nlink = 0` but does **not** free inode 42. The VFS
drops the cached `DEntry "foo"` — the **name edge's `Cap<RNode>` is cut**. The
payload predicate is now `nlinks(0) > 0 ∨ open_refs(1) > 0 = true`; identity
remains pinned by the open edge. Identity outlived its name; payload outlived
its last link.

**[D]** `sys_read` runs exactly as [B] — the open chain is unchanged, so the
unlinked file reads normally. No special case for "reading an orphan."

**[E]** `sys_close` clears the fd, dropping the last `Cap<OpenFile>` → last
`Cap<RNode>(42)` → `RNode` reclaims → `PageContainer` reclaims → predicate
reaches false → VFS calls `FsOps::destroy_inode(42)`; tmpfs frees the inode and
pages. Identity and payload, parted at [C], both reclaim at the moment the last
of the two predicates goes false — driven by reference types reaching zero (with
EBR's two-epoch deferral), not by a sweep or flag.

---

## 12. Type and Source Reference

**Core entities** (`crates/tx-subsystems/src/vfs/structure.rs`)
- `FsObjectId` :95 · `Credential` :124 · `InodeKind`/`InodeMeta` :180,227 ·
  `InlineName` :324 · `RNodeBacking` :480 · `StructPayload` :546 · `RNode` :576 ·
  `DEntry` :866 · `OpenFileBacking` :1016 · `OpenFile` :1154 · `OpenFileFlags` :430 ·
  `OpenFileIoctl` :459 · fcntl overrides + `flags()` :1170 ·
  wait accessors / `fire_read_wait` / `fire_write_wait` :727,808,816

**Execution / backend** (`crates/tx-subsystems/src/vfs/execution.rs`)
- `FsOps` trait :66 · `unlink` no-destroy contract :96 · `MountOutput` :318 ·
  `OpenFile::step_read` :334 · `step_lseek` :469 · `step_write` :591

**Page backing / page cache** (`crates/tx-subsystems/src/page_backed/`)
- `FsPageBacking` `fs_page_backing.rs:34` · `PageContainer`/`PageContainerKind`
  `mod.rs:390,279` · `materialize_page` `mod.rs:759` · fetch coalescing
  `mod.rs:916,877,1020` · `step_read`/`step_write`/`step_range`/`grow_size_to`
  `mod.rs:1239,1265,1305,1223` · user-buffer copy `user_buffer.rs:15,285,421` ·
  `step_fsync` `lifecycle.rs:92` · mmap link `vm/structure/types.rs:353,1072`

**Special files / fd zoo**
- pipe `crates/tx-subsystems/src/pipe/mod.rs:131,88,893,939` · tty exec
  `crates/tx-subsystems/src/tty/execution/` · char device
  `crates/tx-subsystems/src/device.rs:47,53,14`, devfs `crates/tx-fs/src/devfs/mod.rs` ·
  eventfd `eventfd/mod.rs:121,161` · timerfd `timerfd/mod.rs:81,363` ·
  signalfd `signalfd/mod.rs:105,419` · epoll `epoll/mod.rs:37,59`,
  `crates/tx-shims/src/linux_syscall/epoll.rs`

**Blocking / readiness**
- `RNodeWaitPoints` / `VFS_READABLE` `crates/tx-subsystems/src/vfs/notification.rs:30,26` ·
  `PpollOp` `crates/tx-subsystems/src/vfs/composite.rs:968` ·
  EINTR `crates/tx-shims/src/linux_syscall/io.rs:430`

**Step model** — `StepOutcome` `crates/tx-substrate/src/step/mod.rs:272` ·
  spec `docs/design/02_execution/STEP_MODEL_v1.md` (superseded by `docs/Txv3/03_STEP_MODEL_v2.md`)

**Resolution** (`crates/tx-subsystems/src/vfs/`)
- `WalkMode`/`WalkingState`/`PathResolution`/`IORequest` `resolution/state.rs:19,74,112,134` ·
  `kernel_step` `resolution/step.rs:51` · driver `resolution/driver.rs:34` ·
  `step_walk`/`step_open`/`fs_ops_for` `walker.rs:141,202,277` ·
  dentry cache `structure.rs:923,936` · composite ops `composite.rs:39…787`

**Mount** (`crates/tx-subsystems/src/mount/mod.rs`)
- `MountPayload` :292 · `MountPayloadPin` :373 · `MountIdentity` :413 ·
  `payload_cap()` :481 · `umount`/`clone_ns` :613,635 · `Propagation` :121 ·
  `MountApiFile` :169 · `MountNamespace` :524 · `bind_mount`/`remount` :808,974 ·
  `NsProxy` `crates/tx-subsystems/src/process/nsproxy.rs` ·
  boot `crates/tx-kernel/src/init.rs:557,636,755` ·
  initramfs `crates/tx-fs/src/initramfs.rs:88`

**Syscall layer** (`crates/tx-shims/src/linux_syscall/`)
- `sys_openat` `fs_basic.rs:718` · `sys_close`/`sys_lseek` `fs_basic.rs:1176,1374` ·
  `sys_read`/`sys_read_pagebacked` `io.rs:2179,2104` · `sys_write` `io.rs:1913`

**fd table** — `ProcessPayload.fds` `crates/tx-subsystems/src/process/structure.rs:1148`;
  accessors :514,558,626

**Backends** (`crates/tx-fs/src/`)
- tmpfs `tmpfs/mod.rs:200,224,295` · devfs `devfs/mod.rs:11,376` ·
  procfs `procfs/mod.rs` · ext4/FAT bridges `tx_ext4_bridge.rs`, `fat_bridge.rs`

**Design docs** (`docs/design/`)
- Object model / the split `00_meta-framework/object_model_v2.md` (§2 EBR, §3
  identity/payload, §4 reference hierarchy, §5 monotonicity, §8.1.1 bifurcation) ·
  walker `05_filesystem/VFS_CHECKS_V2.1.md` · mount `05_filesystem/MOUNT_v1.md` ·
  page cache `03_memory-vm/PAGE_BACKED_v1.md` · ext4 plan `05_filesystem/TX_EXT4_PLAN_v1_2.md`
