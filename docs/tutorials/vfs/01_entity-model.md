# Part 1 — The entity model

This chapter introduces txKernel's three VFS entities and the handle type that
points at them. We keep the identity/payload split *implicit* here — naming it
is Chapter 2's job — and focus on getting the objects and their traditional
counterparts straight.

## The three entities

| txKernel | Traditional | What it is |
|---|---|---|
| `RNode` | `struct inode` | A live in-core file object: metadata + a routing tag for where the data lives |
| `DEntry` | `struct dentry` | A name in the tree: a `(name, parent, rnode)` node, cached |
| `OpenFile` | `struct file` | One open of a file: offset, flags, and what it points at |

All three are **zone-allocated** and handed back as a `Cap<T>`. Before we open
them up, meet the handle.

## `Cap<T>`: a handle that pins identity

Throughout the VFS you will see `Cap<RNode>`, `Cap<DEntry>`, `Cap<OpenFile>`.
Read `Cap<T>` as *"a refcounted handle that keeps the identity of a `T` alive."*
It is the rough analogue of holding an in-core reference (`igrab`/`dget`) in
Linux — while you hold a `Cap<T>`, that entity's slot will not be reclaimed.

Two things make it more than an `Arc`:

1. **It pins identity, not necessarily payload.** For some entities a `Cap<T>`
   keeps the whole object alive (identity and payload coincide). For others it
   keeps only the small identity slot alive while the payload can be reclaimed
   independently. Chapter 2 makes this precise; for now, "pins the identity
   slot" is the right mental model.
2. **It is produced by *signing* a zone allocation.** Constructors follow a
   pattern: `RNode::new(...)` builds the value, and `RNode::new_cap(...)` signs
   it into a zone and returns `Cap<RNode>`. Under the hood that is
   `step_engine::sign(value)`.

```rust
// the pattern every VFS entity follows
let rnode: Cap<RNode> = RNode::new_cap(fs_object_id, meta, backing)?;
let dentry: Cap<DEntry> = DEntry::new_cap(name, rnode.clone())?;
```

There is also `Weak<T>` (a non-pinning hint that may have gone stale) and, used
only transiently inside a path walk, `IdentRef<'g, T>` (a borrow valid only
within an epoch guard). Both appear below; Chapter 2 gives the full hierarchy.

## `RNode` — the live inode

`RNode` is the in-core representation of a filesystem object. Here it is, very
nearly verbatim:

```rust
pub struct RNode {
    fs_object_id: FsObjectId,                       // the inode number
    meta: InodeMeta,                                // a stat() snapshot
    backing: RNodeBacking,                          // *where the data lives*
    containing_mount: Option<Weak<MountPayload>>,   // which fs this came from
    wait_points: SpinMutex<Option<RNodeWaitPoints>>,// optional readiness endpoints
}
```

Field by field:

- **`fs_object_id: FsObjectId`** — a `u64` newtype, the inode number within its
  backing store. `FsObjectId::ROOT == 1` by convention. This is the stable
  identity within a filesystem.

- **`meta: InodeMeta`** — an *immutable snapshot* of the stat-able metadata:

  ```rust
  pub struct InodeMeta {
      pub mode: u16,     // S_IFMT kind bits + rwx permission bits
      pub uid: u32,
      pub gid: u32,
      pub size: u64,
      pub atime: Timespec, pub mtime: Timespec, pub ctime: Timespec,
      pub nlinks: u32,
      pub blocks: u64,
      pub flags: u32,
  }
  ```

  The file *kind* is encoded in `mode`'s top bits (the POSIX `S_IFMT`
  convention); `meta.kind()` decodes it into an `InodeKind` enum (`Regular`,
  `Directory`, `Symlink`, `CharDevice`, `BlockDevice`, `Fifo`, `Socket`). Note
  the snapshot is *immutable* — when metadata changes (a write extends the
  file, `chmod` changes the mode), the change goes to the backend, and a fresh
  `InodeMeta` is loaded. The `RNode` is not a mutable cache you patch in place.

- **`backing: RNodeBacking`** — the most important field. It does *not* hold
  the data; it *routes* to wherever the data lives:

  ```rust
  pub enum RNodeBacking {
      PageBacked { pc: Cap<PageContainer> },   // a regular file → page cache
      Directory,                               // entries live as DEntry/backend
      Symlink { target: Box<[u8]> },           // the link text
      StructBacked { payload: StructPayload }, // pipe / tty / socket / device
      Projected { schema: ProjectionSchemaId, key: ProjectionKey }, // procfs/sysfs
  }
  ```

  This is where the inode's payload actually is — and crucially, it is *not in
  the `RNode` allocation itself*. A regular file's bytes live in a separately
  allocated `PageContainer`, pointed at by a `Cap<PageContainer>`. A pipe's
  buffer lives in a `PipePayload`. A procfs file has *no* stored payload at all
  — `Projected` says "synthesise the bytes on read." We will catalogue every
  variant in Chapter 3; the point here is that **`RNode` is the identity half,
  and `backing` is the doorway to the payload half.**

- **`containing_mount: Option<Weak<MountPayload>>`** — a *weak* hint back to the
  filesystem this node came from. The walker upgrades it to find the right
  `FsOps` for the next operation. It is weak because the mount's payload may be
  force-unmounted out from under a live `RNode` (Chapter 5) — the `RNode` must
  not keep the mount's backing device pinned.

- **`wait_points`** — lazily-allocated readiness endpoints for VFS-level
  blocking. Most nodes never use them (pipes/ttys/sockets own their own wait
  sources on the backing object), so they are `None` until requested.

> **Traditional VFS vs txKernel.** A Linux `struct inode` *embeds* `i_mapping`,
> the page cache, in the inode allocation. A txKernel `RNode` *refers* to its
> payload through `backing`. That one indirection is what lets the page cache
> outlive the directory entry (unlink-while-open) and lets the `RNode` be a
> small, uniform identity object regardless of whether it fronts a 4 GB file or
> a zero-byte procfs node.

## `DEntry` — a name in the tree

`DEntry` is the directory-cache entry: it ties a *name* to an *`RNode`* and
records its place in the tree.

```rust
pub struct DEntry {
    name: InlineName,                                   // this component's name
    parent: Option<Cap<DEntry>>,                        // strong: keeps ancestry renderable
    rnode: Cap<RNode>,                                  // strong: the inode this names
    mounted: Option<Weak<MountIdentity>>,               // weak: is something mounted here?
    children: SpinMutex<BTreeMap<InlineName, Weak<DEntry>>>, // weak: the dcache
}
```

- **`name: InlineName`** — the path component, stored inline (up to
  `VFS_NAME_MAX` = 255 bytes, no allocation). `InlineName::new` rejects empty
  names, oversized names, and names containing `/`. The root dentry carries the
  empty `InlineName::ROOT` sentinel, which path-rendering prints as a leading
  `/`.

- **`parent: Option<Cap<DEntry>>`** — a *strong* reference to the parent. This
  keeps the ancestor chain alive so the kernel can always render an absolute
  path for a live dentry (cwd, an open directory). `None` marks a root.

- **`rnode: Cap<RNode>`** — a *strong* reference to the inode this name resolves
  to. Multiple `DEntry`s can point at the same `RNode` — that is exactly what a
  hard link is.

- **`mounted: Option<Weak<MountIdentity>>`** — set when a filesystem is mounted
  on this dentry. The walker checks `mounted_hint()` at each step to know when
  to cross a mount boundary (Chapter 5). Weak, because the mount can be detached.

- **`children: SpinMutex<BTreeMap<InlineName, Weak<DEntry>>>`** — the dentry
  cache. Crucially the children are held **weakly**. This is the inverse of the
  parent edge: parents hold children weakly, children hold parents strongly.

> **Why parent-strong, child-weak?** It breaks the retention cycle. If both
> edges were strong, a directory and its children would form a reference cycle
> and never reclaim. Holding children weakly means the cache does not, by
> itself, keep a subtree alive: a cached child whose last real user is gone
> simply fails to upgrade and is dropped from the map on next access
> (`cached_child` removes dead weak entries lazily). Holding the parent strongly
> means any *live* dentry can still name itself all the way up to the root. This
> is the same reason Linux dentries are reclaimable under memory pressure but a
> dentry you hold stays pathable — txKernel just encodes the policy in the
> reference *types* instead of in shrinker heuristics.

## `OpenFile` — one open of a file

`OpenFile` is the open-file *description*: the per-open state that Linux puts in
`struct file`.

```rust
pub struct OpenFile {
    backing: OpenFileBacking,          // what this fd points at
    offset: AtomicU64,                 // the file position
    readdir_cursor: AtomicU64,         // getdents64 resume cursor
    flags: OpenFileFlags,              // read/write/append/cloexec/nonblock/packet
    nonblocking_override: AtomicI8,    // runtime fcntl(F_SETFL) O_NONBLOCK
    packet_override: AtomicI8,         // runtime fcntl(F_SETFL) packet mode
    opendir_dentry: Option<Cap<DEntry>>, // best-effort hint for fchdir
}
```

- **`backing: OpenFileBacking`** — a fd is not always a VFS file. The backing
  enum is a discriminated union over every fd kind:

  ```rust
  pub enum OpenFileBacking {
      Rnode { rnode: Cap<RNode> },               // the normal case: a VFS file
      Pidfd { process: Cap<ProcessIdentity> },   // pidfd_open
      Ufd { ufd: Cap<UserfaultFd> },             // userfaultfd
      SignalFd { sfd: Cap<SignalFd> },           // signalfd
      Eventfd { efd: Cap<EventFd> },             // eventfd
      Timerfd { tfd: Cap<TimerFd> },             // timerfd
      // … epoll, posix_mq, aio, socketpair, mount-api, kernel-object …
  }
  ```

  Most files are `Rnode { rnode }`. The other variants are how Linux's "every
  fd-returning syscall produces something that quacks like a file" surface is
  modeled without forcing eventfd or a pidfd to pretend to have an inode. The
  VFS-only step methods (`step_read`, `step_write`, `step_lseek`) operate on the
  `Rnode` shape; other shapes branch on `backing` first.

- **`offset: AtomicU64`** — the file position. It is *atomic* and lives in the
  `OpenFile`, which matters because `dup` and `fork` *share the same
  `Cap<OpenFile>`*. That gives the POSIX semantic exactly: two fds from a `dup`
  share one offset, because they are two `Cap` clones pointing at one
  `OpenFile`. `readdir_cursor` works the same way for directory streams.

- **`flags: OpenFileFlags`** — the decoded open mode:

  ```rust
  pub struct OpenFileFlags {
      pub read: bool, pub write: bool, pub append: bool,
      pub cloexec: bool, pub nonblocking: bool, pub packet: bool,
  }
  ```

- **`opendir_dentry`** — a best-effort `Cap<DEntry>` stashed at open time so
  `fchdir(fd)` can recover which directory the fd names. `None` for non-VFS
  shapes.

> **Traditional VFS vs txKernel.** Linux's `struct file` holds `f_pos`,
> `f_flags`, `f_count`, and `f_op`. txKernel's `OpenFile` holds the same
> per-open state, but note what it does *not* hold: no embedded vtable. The
> operations come from the `RNode`'s `backing`, reached through the `Cap<RNode>`
> in `OpenFileBacking::Rnode`. The open file points at identity; identity routes
> to capability and payload.

## How they fit together

For an open regular file `/tmp/foo`, the live object graph is:

```
ProcessPayload.fds[fd] ─► Cap<OpenFile>
                              │ backing: Rnode
                              ▼
                          Cap<RNode>  ◄──────────── Cap<RNode> ◄── DEntry "foo"
                              │ backing: PageBacked          (the name in /tmp)
                              ▼
                          Cap<PageContainer>   ← the payload (the actual bytes)
```

Two independent chains reach the same `RNode`: the **fd table → OpenFile**
chain (this open) and the **dentry tree** chain (this name). They were created
at different times and they die at different times. The `RNode` (identity) lives
as long as *either* chain holds a `Cap<RNode>`. The `PageContainer` (payload)
lives as long as anything still needs the bytes.

When you `unlink("/tmp/foo")`, the `DEntry` "foo" is removed from `/tmp`'s
children and its edge to the `RNode` drops — but the `OpenFile` still holds its
`Cap<RNode>`, so the `RNode` and its `PageContainer` stay alive. That is the
unlinked-but-open file, and now you can see it is not a trick: it is just *one
of the two chains being cut while the other holds on.* Chapter 2 explains why
the reference types guarantee this works, and Chapter 10 traces it live.

## Source anchors

- `FsObjectId`: `crates/tx-subsystems/src/vfs/structure.rs:95`
- `Credential`: `crates/tx-subsystems/src/vfs/structure.rs:124`
- `InodeKind` / `InodeMeta`: `crates/tx-subsystems/src/vfs/structure.rs:180,227`
- `InlineName`: `crates/tx-subsystems/src/vfs/structure.rs:324`
- `RNodeBacking`: `crates/tx-subsystems/src/vfs/structure.rs:480`
- `StructPayload`: `crates/tx-subsystems/src/vfs/structure.rs:546`
- `RNode`: `crates/tx-subsystems/src/vfs/structure.rs:576`
- `DEntry`: `crates/tx-subsystems/src/vfs/structure.rs:866`
- `OpenFile` / `OpenFileBacking` / `OpenFileFlags`: `crates/tx-subsystems/src/vfs/structure.rs:1154,1016,430`
- fd table: `crates/tx-subsystems/src/process/structure.rs:1131`
