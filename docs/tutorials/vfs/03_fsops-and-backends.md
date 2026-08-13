# Part 3 — `FsOps`: the backend boundary

A VFS is only useful because the same syscalls drive many filesystems. The thing
that makes that work is the *backend operations vtable*. In Linux it is
`inode_operations` + `file_operations`. In txKernel it is a single trait,
`FsOps`, dispatched as `Arc<dyn FsOps>`. This chapter is that boundary: the
trait, the execution model its methods speak, and a gallery of the four backings
that implement it.

## The trait

`FsOps` is "the canonical v3 filesystem operation vtable" (`execution.rs:46`).
It is the identity-and-namespace side of a filesystem: name lookup, inode
metadata, directory mutation. Here is the core surface, lightly trimmed:

```rust
pub trait FsOps: Send + Sync + 'static {
    fn lookup(&self, parent: FsObjectId, name: &[u8], guard: &Guard)
        -> StepOutcome<FsObjectId, NoProgress>;

    fn load_inode_meta(&self, id: FsObjectId, guard: &Guard)
        -> StepOutcome<InodeMeta, NoProgress>;
    fn serialize_inode_meta(&self, id: FsObjectId, meta: &InodeMeta, guard: &Guard)
        -> StepOutcome<(), NoProgress>;

    fn create_inode(&self, parent: FsObjectId, name: &[u8], mode: u16,
                    cred: &Credential, guard: &Guard)
        -> StepOutcome<(FsObjectId, InodeMeta), NoProgress>;
    fn mkdir(&self, parent: FsObjectId, name: &[u8], mode: u16,
             cred: &Credential, guard: &Guard)
        -> StepOutcome<(FsObjectId, InodeMeta), NoProgress>;
    fn symlink(&self, parent: FsObjectId, name: &[u8], link_target: &[u8],
               cred: &Credential, guard: &Guard)
        -> StepOutcome<(FsObjectId, InodeMeta), NoProgress>;

    fn unlink(&self, parent: FsObjectId, name: &[u8], target: FsObjectId, guard: &Guard)
        -> StepOutcome<(), NoProgress>;
    fn rmdir(&self, parent: FsObjectId, name: &[u8], target: FsObjectId, guard: &Guard)
        -> StepOutcome<(), NoProgress>;
    fn link(&self, parent: FsObjectId, name: &[u8], target: FsObjectId, guard: &Guard)
        -> StepOutcome<(), NoProgress>;
    fn rename(&self, old_parent: FsObjectId, old_name: &[u8],
              new_parent: FsObjectId, new_name: &[u8], guard: &Guard)
        -> StepOutcome<(), NoProgress>;

    fn readdir(&self, id: FsObjectId, cursor: DirCursor, guard: &Guard)
        -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress>;

    fn destroy_inode(&self, id: FsObjectId, guard: &Guard)
        -> StepOutcome<(), NoProgress>;

    // defaulted (ENOSYS unless overridden):
    fn read_link(&self, id: FsObjectId, guard: &Guard) -> StepOutcome<Box<[u8]>, NoProgress> { … }
    fn step_chmod(&self, …) -> StepOutcome<(), NoProgress> { … }
    fn step_chown(&self, …) -> StepOutcome<(), NoProgress> { … }
    fn step_read_projected(&self, id: FsObjectId, off: u64, buf: &mut [u8], guard: &Guard)
        -> StepOutcome<u64, NoProgress> { … }
    fn step_write_projected(&self, id: FsObjectId, off: u64, bytes: &[u8], guard: &Guard)
        -> StepOutcome<u64, NoProgress> { … }
}
```

A few things to notice immediately:

- **Everything speaks `FsObjectId`, not `RNode`.** The backend never sees a
  `Cap<RNode>` or a `DEntry`. It deals in inode *numbers* (`FsObjectId`) and
  immutable metadata snapshots (`InodeMeta`). The VFS core owns the live
  identity objects; the backend owns the storage. This is the
  **stateless-per-inode** rule (`TX_EXT4_PLAN_v1_2.md`): a backend is a pure
  function from `(inode number, operation)` to `(result, new state in the
  store)`. It does not cache live node objects; the VFS layer does that, in
  `RNode`s.

- **Defaults give you a backend for free.** A projection-only filesystem
  (procfs) implements `lookup`/`load_inode_meta`/`readdir`/`step_read_projected`
  and inherits `ENOSYS` for `mkdir`/`symlink`/`link`. A read-only device
  namespace inherits the mutation methods it does not want. You implement what
  your filesystem actually supports.

- **`unlink` does not free the inode.** Read the contract in the source
  (`execution.rs:96`): unlink removes *one name* and drops the link count, but
  "implementations must not destroy the inode payload here: open files, live
  RNodes, and page-cache state may still address `target` after the last name
  disappears." Reclamation is a *separate* call, `destroy_inode`, made by the
  VFS only when its liveness predicate says no payload references remain. This
  is Motivation 1 from Chapter 2, enforced at the trait boundary: **the split
  between "remove the name" and "free the payload" is two different methods.**

## The companion trait: `FsPageBacking`

`FsOps` is the identity/namespace side. The *bytes* of a regular file go through
a second trait, `FsPageBacking` (`page_backed/fs_page_backing.rs:34`):

```rust
pub trait FsPageBacking: Send + Sync + 'static {
    fn fetch_page(&self, id: FsObjectId, offset: u64, guard: &Guard)
        -> StepOutcome<Frame, NoProgress>;
    fn flush_page(&self, id: FsObjectId, offset: u64, frame: &Frame, guard: &Guard)
        -> StepOutcome<(), NoProgress>;
    fn truncate(&self, id: FsObjectId, new_size: u64, guard: &Guard)
        -> StepOutcome<(), NoProgress>;
    fn fsync_file(&self, id: FsObjectId, guard: &Guard) -> StepOutcome<(), NoProgress>;
    // sync_filesystem, fallocate, supports_reflink — defaulted
}
```

This is the analogue of Linux's `address_space_operations` (`readpage`,
`writepage`): the page-cache-facing surface. A page-cache-backed filesystem
implements *both* traits; a procfs implements only `FsOps` (its files have no
pages). The two are deliberately separate — they do not subsume each other —
because the page-cache surface counts pages and the namespace surface does not.

> **Traditional VFS vs txKernel.** Linux splits filesystem behaviour across
> `super_operations`, `inode_operations`, `file_operations`, `dentry_operations`,
> and `address_space_operations`. txKernel collapses the namespace/inode/file
> operations into one `FsOps` trait and keeps the page-cache surface as a
> second, `FsPageBacking`. The split that survives is the one that *matters* for
> lifetime: namespace ops vs payload (page) ops.

## How a backend is mounted: `MountOutput`

A filesystem hands itself to the VFS by producing a `MountOutput`
(`execution.rs:318`):

```rust
pub struct MountOutput {
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}
```

Both trait objects, plus the root inode's number and metadata. The mount layer
wraps these in a `MountPayload` (Chapter 5). For a backend like tmpfs that
implements both traits on one struct, the two `Arc`s point at the *same*
instance, so both surfaces observe the same state.

## The execution model: why every method returns `StepOutcome`

You have seen `StepOutcome<T, NoProgress>` on every method. This is txKernel's
**step model**, and understanding it is what lets a *synchronous* backend
participate in an *asynchronous* kernel. (The reactor tutorial covers the
executor side; here is just what a backend author must know.)

A **step** is a synchronous, bounded unit of work that takes its own epoch
guard and returns exactly one outcome from a closed four-variant algebra
(`tx-substrate/src/step/mod.rs:272`):

```rust
pub enum StepOutcome<T, P> {
    Continue { progress: P },              // made progress, call me again
    Yield { progress: P, shape: YieldShape }, // must wait — here's what on
    Done(T),                               // finished, here's the result
    Err(Errno),                            // failed
}
```

`FsOps` methods use `P = NoProgress`: they are one-shot identity-side queries.
`tmpfs::lookup` either finds the name (`Done(id)`), doesn't (`Err(ENOENT)`), or
— for a backend that had to wait on storage — `Yield`s. Because `NoProgress` is
the progress type, an `FsOps` method never accumulates partial work across
calls; `readdir` returns *one* entry and a cursor, and the caller re-invokes
with the cursor to get the next (the cursor is an input, not progress).

The byte-moving surface (`FsPageBacking::fetch_page`) is where `Yield` earns its
keep: a disk read that must wait for I/O returns `Yield { shape:
OnWaitSource(...) }`, naming the wait source that will wake it. tmpfs never
waits (its pages are already in memory), so it only ever returns `Done`.

### Bridging synchronous steps into async syscalls: `drive`

A syscall like `read` is an `async fn`. A step is synchronous. The bridge is the
**driver**: a small executor (`drive` / `drive_oneshot`) that calls a step
repeatedly:

```
loop {
    match op.step(ctx) {
        Continue { .. } => continue,                 // immediately call again
        Yield { shape, .. } => {                     // register on the wait source,
            register_waker(shape);                   //   return Pending to the reactor,
            return Poll::Pending;                    //   resume here when woken
        }
        Done(v) => return Poll::Ready(Ok(v)),
        Err(e)  => return Poll::Ready(Err(e)),
    }
}
```

So the backend author writes ordinary synchronous code that returns
`StepOutcome`, and the driver turns a `Yield` into a real `.await` suspension in
the syscall future. The backend never touches `async`, never holds a guard
across a suspension (the guard is taken and released *inside* one step), and
never blocks a hart — a `Yield` parks the syscall future and frees the CPU.
This is why every `FsOps`/`FsPageBacking` method takes `&Guard`: the guard is
the step's, scoped to that one synchronous call.

> **Traditional VFS vs txKernel.** A Linux filesystem method blocks the calling
> thread on a wait queue when it must wait for I/O; the kernel stack parks. A
> txKernel backend method *returns* `Yield` instead of blocking; nothing parks,
> the syscall future suspends, and the hart runs other work. Same conceptual
> "wait for the disk," different mechanism — and the backend code looks
> synchronous either way.

## The backing gallery

Recall `RNodeBacking` from Chapter 1 — the routing tag on every `RNode`. Each
variant corresponds to a *style* of backend. Here is the gallery: four
filesystems, four ways of being a file.

Before the individuals, the shape they all plug into. One mount carries *two*
trait objects; the `RNode`'s `backing` tag decides which one (and which method)
a given operation reaches:

```
   Cap<RNode>
     │  .backing()
     ▼
  ┌────────────────────────────────────────────────────────────────┐
  │ RNodeBacking                                                     │
  │   PageBacked{pc} ───────┐   Projected{schema,key} ──┐  StructBacked{..}
  └─────────┬───────────────┼──────────────┬────────────┼───────────┘
            │ identity ops   │ byte ops      │ read       │ read/write
            │ (lookup,mkdir) │ (read,write)  │            │
            ▼                ▼               ▼            ▼
   ┌──────────────┐  ┌──────────────┐  fs_ops().      pipe::/tty::/
   │ Arc<dyn      │  │ Arc<dyn      │  step_read_      binding.ops.read
   │   FsOps>     │  │ FsPageBacking│  projected       (Chapter 8)
   └──────┬───────┘  └──────┬───────┘
          └────────┬────────┘
                   ▼  both held by one mount
            ┌──────────────────────────────┐
            │ MountPayload                 │
            │   fs_ops:          Arc<dyn FsOps>          │
            │   fs_page_backing: Arc<dyn FsPageBacking>  │
            │   backing: Option<Arc<dyn BlockDevice>>    │
            └──────────────────────────────┘
```

The identity side (`FsOps`) and the byte side (`FsPageBacking`) are separate
traits because they answer separate questions — "what is named here?" vs "what
bytes are at this offset?" — and a backend may implement one without the other
(procfs has no pages; a future raw block device has no namespace). For tmpfs and
ext4 both `Arc`s point at one object, so they observe one state.

### tmpfs — `PageBacked`, the worked example

tmpfs is the simplest *complete* backend: an in-memory filesystem implementing
both `FsOps` and `FsPageBacking`. Its whole state is an inode table behind a
lock (`tmpfs/mod.rs:200`):

```rust
pub struct Tmpfs {
    state: TmpfsSpinMutex<TmpfsState>,    // BTreeMap<FsObjectId, TmpfsInode>
    next_object_id: AtomicU64,
    next_dirent_cookie: AtomicU64,
}

struct TmpfsInode { meta: InodeMeta, payload: TmpfsPayload, nlink: u32 }

enum TmpfsPayload {
    Directory(BTreeMap<InlineName, TmpfsDirEntry>),
    RegularFile { container: Cap<PageContainer>, size: u64 },
    Symlink(Arc<[u8]>),
}
```

`lookup` is exactly what you'd expect — validate the name, find the parent,
index its directory map (`tmpfs/mod.rs:296`):

```rust
fn lookup(&self, parent: FsObjectId, name: &[u8], _guard: &Guard)
    -> StepOutcome<FsObjectId, NoProgress>
{
    let inline = InlineName::new(name)?;                 // reject bad names
    let state = self.state.lock();
    let parent_inode = state.inodes.get(&parent).ok_or(ENOENT)?;
    let Directory(children) = &parent_inode.payload else { return err(ENOTDIR) };
    match children.get(&inline) {
        Some(entry) => StepOutcome::done(entry.object_id),
        None => StepOutcome::err(ENOENT),
    }
}
```

A regular file's bytes are *not* in the `TmpfsInode` — they are in a
`Cap<PageContainer>`, the same page-cache container that an `RNode`'s
`PageBacked { pc }` points at. That is the split inside the backend: the inode
table holds identity (`meta`, `nlink`) and a *handle* to payload (the container
cap). `new_root` (`tmpfs/mod.rs:224`) produces the `MountOutput` with both trait
objects pointing at the same `Arc<Tmpfs>`.

That handle-to-payload split has a consequence `load_inode_meta` has to respect:
the file's *length* lives in the `PageContainer`, not in the cached `meta.size`.
A `write` grows the container (`pc.grow_size_to`, Chapter 7) without touching the
inode record, so `stat` must read the size back from the container — and it must
do so *after* dropping the mount-wide lock, because the container has its own
lock (`tmpfs/mod.rs:323`):

```rust
fn load_inode_meta(&self, fs_object_id: FsObjectId, _guard: &Guard)
    -> StepOutcome<InodeMeta, NoProgress>
{
    let snapshot = {
        let state = self.state.lock();
        let Some(inode) = state.inodes.get(&fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT);
        };
        // For regular files the authoritative size lives in the PageContainer;
        // snapshot the container cap here, read its size after dropping `state`.
        let size_source = match &inode.payload {
            TmpfsPayload::RegularFile { container, .. } => Some(container.clone()),
            _ => None,
        };
        InodeMetaSnapshot { meta: inode.meta, nlink: inode.nlink, size_source }
    };                                              // ← mount lock released here
    StepOutcome::done(inode_meta_from_snapshot(snapshot))  // reads size_source.size_bytes()
}
```

Two lock disciplines in one function: take the mount state lock just long enough
to *clone the container cap* into the snapshot, release it, then read the size
through that cap. Holding both locks at once would invert the lock order the
write path uses and risk deadlock; the snapshot decouples them.

`create_inode` is the other end — making a node. Its job is to validate, derive
the kind from the `S_IFMT` bits (so one method serves `mknod`'s regular/FIFO/
device/socket cases), apply the parent's setgid inheritance, allocate an id, and
insert into the parent's directory map (`tmpfs/mod.rs:378`, abridged to the
control flow):

```rust
fn create_inode(&self, parent, name, mode, cred, _guard)
    -> StepOutcome<(FsObjectId, InodeMeta), NoProgress>
{
    let inline = InlineName::new(name)?;                 // reject empty / oversized / "/"
    let kind = match mode & S_IFMT {                     // S_IFMT bits choose the kind
        0 | S_IFREG => InodeKind::Regular,
        S_IFIFO => InodeKind::Fifo,
        S_IFCHR => InodeKind::CharDevice,
        S_IFBLK => InodeKind::BlockDevice,
        S_IFSOCK => InodeKind::Socket,
        _ => return StepOutcome::err(Errno::EINVAL),
    };
    // observe parent: must exist, be a directory, not already contain `inline`
    let (parent_setgid, parent_gid) = { /* lock state; check; read setgid+gid */ };
    // ... allocate FsObjectId, build InodeMeta (setgid inheritance from parent),
    //     create RegularFile{container: empty PageContainer} or Fifo/… payload,
    //     insert into parent Directory map, bump parent nlink for dirs ...
    StepOutcome::done((new_id, new_meta))
}
```

The pattern every mutating `FsOps` method follows: *validate the name, observe
the parent under the lock, derive the new state, commit it.* Note what is
absent — no `RNode`, no `DEntry`, no `Cap`. The backend returns a *number and a
metadata snapshot*; the VFS core materialises the live identity objects from
them (Chapter 4's `materialise_child`). The backend stays stateless-per-inode.

### procfs — `Projected`, files with no stored bytes

procfs files have no payload at all. `/proc/<pid>/stat` is computed when you
read it. The `RNode` carries `RNodeBacking::Projected { schema:
ProjectionSchemaId::Procfs, key }`, and reads route to the defaulted
`step_read_projected` method, which the procfs backend overrides to synthesise
bytes from live kernel state keyed by `key.object_id`.

So procfs implements the *namespace* side of `FsOps` (`lookup`, `readdir`,
`load_inode_meta`) and the *projected read* hook, and nothing else — no
`FsPageBacking`, no `create_inode`. Its "payload" is a pure function of kernel
state at read time. This is the cleanest demonstration of the split's far end:
an entity that is *all identity, zero stored payload.*

### devfs — `StructBacked`, deferring to a device subsystem

A device node (`/dev/null`, `/dev/urandom`, a tty) is a name in the filesystem
whose behaviour lives in a *device driver*, not in the filesystem. devfs models
this with `RNodeBacking::StructBacked { payload: StructPayload::CharDevice(...) }`
(and `Tty`, `BlockDevice`, …). The `StructPayload` carries a reference to the
device binding:

```rust
pub enum StructPayload {
    Tty(Cap<TtyIdentity>),
    CharDevice(&'static CharDeviceBinding),
    BlockDevice(&'static BlockDeviceRegistration),
    Pipe { payload: Cap<PipePayload>, side: PipeSide },
    Socket { identity: Cap<SocketIdentity> },
    // …
}
```

A `read` of `/dev/urandom` lands in `OpenFile::step_read`, sees
`StructBacked { CharDevice(binding) }`, and calls `binding.ops.read(...)` — the
device driver's method (Chapter 6). The filesystem provided the *name and
identity*; the device subsystem provides the *payload behaviour*. devfs's
`FsOps` is mostly a directory listing of statically-registered device bindings.

### ext4 / FAT bridge — `PageBacked` over a real block device

The on-disk filesystems are the full story: identity in inodes on disk, payload
in data blocks on disk, mediated by a page cache. The bridges
(`tx_ext4_bridge.rs`, `fat_bridge.rs`) adapt the `tx-ext4` / `tx-fat` crates to
the two traits. `FsOps::lookup` reads directory blocks; `FsPageBacking::
fetch_page` reads a data block from the `Arc<dyn BlockDevice>` into a `Frame`,
returning `Yield { OnWaitSource }` while the I/O is outstanding — the one place
in the gallery where `Yield` is real. The "stateless-per-inode" rule matters
most here: the bridge does not keep live node objects, so the VFS's `RNode`
cache and the page cache are the *only* in-core caches, with a single
reclamation policy.

| Backend | `RNodeBacking` | Implements | Payload lives… | `Yield`s? |
|---|---|---|---|---|
| tmpfs | `PageBacked` | `FsOps` + `FsPageBacking` | in-memory `PageContainer` | no |
| procfs | `Projected` | `FsOps` (+ projected read) | nowhere — synthesised | no |
| devfs | `StructBacked` | `FsOps` | in the device subsystem | device-dependent |
| ext4 / FAT | `PageBacked` | `FsOps` + `FsPageBacking` | on disk, via page cache | yes (disk I/O) |

The lesson of the gallery: `FsOps` is *one* boundary, but "what is a file's
payload" has four different answers, and `RNodeBacking` is the tag that records
which answer applies — so the read/write path (Chapter 6) can dispatch to the
right place while the identity object stays uniform.

## Source anchors

- `FsOps` trait: `crates/tx-subsystems/src/vfs/execution.rs:66`
- `unlink` "do not destroy inode" contract: `crates/tx-subsystems/src/vfs/execution.rs:96`
- `MountOutput`: `crates/tx-subsystems/src/vfs/execution.rs:318`
- `FsPageBacking` trait: `crates/tx-subsystems/src/page_backed/fs_page_backing.rs:34`
- `StepOutcome` four variants: `crates/tx-substrate/src/step/mod.rs:272`
- Step model spec: `docs/design/02_execution/STEP_MODEL_v1.md` (superseded by `docs/Txv3/03_STEP_MODEL_v2.md`)
- tmpfs: `crates/tx-fs/src/tmpfs/mod.rs:200` (struct), `:224` (`new_root`), `:295` (`impl FsOps`)
- devfs `StructBacked`: `crates/tx-fs/src/devfs/mod.rs:11,376`
- `StructPayload`: `crates/tx-subsystems/src/vfs/structure.rs:546`
- ext4 / FAT bridges: `crates/tx-fs/src/tx_ext4_bridge.rs`, `crates/tx-fs/src/fat_bridge.rs`
