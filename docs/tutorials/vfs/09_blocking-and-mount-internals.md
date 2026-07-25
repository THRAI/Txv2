# Part 9 — Blocking, readiness, and mount internals

Two functional areas the earlier chapters deferred. First: what actually happens
when a `read` *blocks* — the wait-source machinery behind every "yield on
readable" we have hand-waved. Second: the mount features beyond "attach a
filesystem" — propagation, bind mounts, namespaces, the new mount API, and how
the very first filesystem gets mounted at boot. Both are where the VFS meets the
rest of the kernel.

## Part A — Blocking and readiness

### The wait-source, and `RNodeWaitPoints`

Every "block until ready" in this series — pipe read on empty, tty read on no
input, eventfd read on zero — ends in the same primitive: a **wait-source**. A
wait-source is a registry-addressable wake point; a task parks on it by
subscribing its mailbox, and a producer *fires* it to wake every subscriber.
(The reactor tutorial covers the executor side; here we use it.)

For VFS files that need readiness, the `RNode` lazily allocates a pair
(`vfs/notification.rs:30`):

```rust
struct RNodeWaitPoints {
    read_source: Arc<WaitSource>,  read_source_id: u64,  read_channel: Channel,
    write_source: Arc<WaitSource>, write_source_id: u64, write_channel: Channel,
}
```

One source for "bytes available to read," one for "space available to write." The
allocation is lazy (`RNode.wait_points: Mutex<Option<…>>`) because most nodes —
pipes, ttys, eventfds — own their wait-sources on the *backing* object
(`PipePayload.reader_wait_source`, etc.), so a path lookup of a plain file
should not allocate two global registry slots it will never use. Chapter 1 noted
this; here is why it matters: long LTP runs walk millions of paths, and eager
allocation would be unbounded registry pressure.

Readiness is a two-bit mask (`vfs/notification.rs:26`):

```rust
pub const VFS_READABLE: u64 = 0x1;
pub const VFS_WRITABLE: u64 = 0x2;
```

A producer wakes waiters by firing (`vfs/structure.rs:808`):

```rust
fn fire_read_wait(&self, mask: u64) -> usize   // wake readers: notify WaitSource + legacy Channel
fn fire_write_wait(&self, mask: u64) -> usize   // wake writers
```

These fire *both* the new `WaitSource` and the legacy `Channel` in tandem — a
migration seam where two readiness mechanisms coexist.

### Park and wake, end to end

Trace a blocking pipe read (the pieces are all from Chapter 8):

```
1. read(fd) → OpenFile::step_read → pipe::step_read
2. ring empty, writers present, blocking
   → step_read returns StepOutcome::Yield { shape: OnWaitSource { source: reader_wait_source_id, interests: READABLE } }
3. the driver (Chapter 3's `drive`) sees Yield, subscribes the task's TaskMailbox
   to reader_wait_source_id, and returns Poll::Pending → the syscall future suspends
4. ... hart runs other work; no stack parked ...
5. a writer calls write(fd) → pipe::step_write fills the ring,
   then notify_readable(reader_wait_source) fires the source
6. every subscribed mailbox is woken; the reader's syscall future is re-polled
7. step_read runs again, finds bytes, drains, returns Done(n)
```

The crucial property, shared with the whole kernel: between steps 3 and 6 there
is **no parked kernel stack** — the blocked read is a suspended future plus a
wait-source subscription, nothing more. A `Yield` carries a `YieldShape` naming
*what* to wait on (`OnWaitSource { source, interests }`), so the driver knows
which registry slot to subscribe.

As an object graph, the wait side and the wake side meet at the registry:

```
   blocked reader                              waking writer
       │ step_read → Yield{OnWaitSource}            │ step_write fills ring
       ▼                                            ▼
   wait_until_readable(source_id)            notify_readable(channel, source)
       │ = yield_on_wait_source(            ┌────────┴─────────┐
       │     EMPTY, source_id, READABLE)    │                  │
       ▼                                    ▼                  ▼
   driver subscribes TaskMailbox      fire_legacy_channel   notify_v3_source
   to source_id in WAIT REGISTRY ◀────  (Channel, mask)     (WaitSource, mask)
       │                                    └────────┬─────────┘
       │         ┌──────────────────────────────────┘
       ▼         ▼  wakes every mailbox subscribed to source_id
   TaskMailbox re-polled → step_read retries → Done(n)
```

The two arrows out of `notify_readable` are the migration seam in the flesh: the
primitive fires *both* a legacy `Channel` and the v3 `WaitSource` so old and new
waiters both wake during the transition. Faithfully (`pipe/notification.rs:68`):

```rust
fn notify_readable(channel: &Channel, source: &Arc<WaitSource>) {
    wait_routing::fire_legacy_channel(channel, PIPE_READABLE);   // legacy D2/D4 path
    wait_routing::notify_v3_source(source, PIPE_READABLE);       // v3 wait-source path
}
fn wait_until_readable(source_id: u64) -> StepOutcome<usize, ByteProgress> {
    step_engine::yield_until_readable(source_id, PIPE_READABLE)  // = yield_on_wait_source(EMPTY, id, mask)
}
```

`wait_until_readable` is a thin wrapper over `StepOutcome::yield_on_wait_source`
with the `PIPE_READABLE` mask — the same `Yield`-with-`YieldShape` every backend
returns. Every special file has its own `notification` module (`pipe::`, `tty::`,
`eventfd::`, `signalfd::`) with this exact pair, differing only in the mask
constant and whether the channel is optional — so "block until readable" is one
shape repeated across the zoo, not a per-type mechanism.

### `poll` / `select` / `epoll`

Readiness query syscalls build on the same sources. The single-fd skeleton is
`PpollOp` (`vfs/composite.rs:968`):

```rust
pub struct PpollOp { wait_source_id: WaitSourceId, interests: InterestMask, timeout_ms: Option<u64>, started: bool }
```

Its first `step` yields on the fd's wait-source; the resume returns the ready
fd. `epoll` (Chapter 8) fans this out: `epoll_wait` (`linux_syscall/epoll.rs`)
calls `collect_ready_events` over the registered entries, and if none are ready
parks on every registered fd's wait-source at once via `wait_for_epoll_wake`,
mapping each backing kind to its readiness predicate (`ready_mask_for_entry`:
pipe levels, eventfd counter, socket poll mask, timerfd expirations, …). So
`epoll` is not a new mechanism — it is a multiplexer over the wait-sources the
individual files already expose.

### Signal interruption: `EINTR`

A blocked VFS wait must also wake for a *signal* — that is `EINTR`. The blocking
syscalls register on two things at once: the fd's wait-source **and** the
thread's signal mailbox (`linux_syscall/io.rs:430`, `await_any_wait_source`). If
the signal mailbox fires first, the wait returns "interrupted"; the syscall arm
checks `select_next_signal` and, if a signal is deliverable, returns `EINTR`
instead of retrying (or restarts the syscall if `SA_RESTART` is set). This is
why readiness and signals share the wait-source substrate: a blocked read has to
lose the race to *either* data or a signal, and both are wake events on the same
task.

> **Traditional VFS vs txKernel.** Linux blocks a read on a wait-queue
> (`wait_event_interruptible`), and signal delivery wakes the queue with
> `TIF_SIGPENDING`, producing `-ERESTARTSYS`/`EINTR`. txKernel's wait-source +
> mailbox is the same shape — a blocked operation subscribed to a wake point,
> woken by data or signal — but the blocked operation is a suspended future, and
> "subscribe to both the data source and the signal mailbox" is explicit rather
> than implicit in the wait-queue plus pending-signal check.

## Part B — Mount internals

Chapter 5 covered the `MountIdentity`/`MountPayload` split and force-umount.
Here are the features that ride on top.

### Propagation (shared / private / slave)

A mount has a propagation type controlling whether mount/unmount events
*propagate* to related mounts (`mount/mod.rs:121`):

```rust
pub enum Propagation { Private, Shared, Slave, Unbindable }
```

stored on the payload as atomics with a peer-group id (`mount/mod.rs:421`):

```rust
propagation: AtomicU64,   // Propagation::to_bits()
peer_group: AtomicU64,    // non-zero: this mount shares with peers in the same group
```

- **Private** (default) — events don't propagate.
- **Shared** — members of a peer group replicate mounts to each other (mount
  something under one, it appears under all peers). `set_propagation` +
  `allocate_peer_group_id` wire this.
- **Slave** — receives propagation from a master but doesn't send (the current
  tree records the relationship; receive-side replication is a follow-up).
- **Unbindable** — private and refuses to be bind-mounted.

This is the machinery behind `mount --make-shared` and the reason a mount in one
container can be made to (not) appear in another.

### Bind mounts and remount

A **bind mount** makes an existing subtree appear at another path *without a new
backend*. The whole function is short, and every line is load-bearing
(`mount/mod.rs:808`):

```rust
fn bind_mount(source_dentry, target_dentry, target_parent_payload, guard)
    -> Result<BindMountOutput, Errno>
{
    let source_payload =                                       // reuse the SOURCE's payload
        walker::mount_payload_for(&source_dentry, guard).ok_or(ENODEV)?;
    let source_rnode = source_dentry.rnode().clone();          // new mount's root = source subtree
    let target_fs_object_id = target_dentry.rnode().fs_object_id();

    let mount_id = allocate_mount_id();
    let mount_cap = MountIdentity::new_cap(                     // fresh IDENTITY...
        mount_id, Some(target_dentry), source_rnode, None,
        source_payload,                                        // ...bound to the SOURCE's PAYLOAD
        MountFlags::empty(),
    ).map_err(|_| ENOMEM)?;

    register_mount(target_parent_payload, target_fs_object_id, mount_cap.clone());
    Ok(BindMountOutput { mount: mount_cap })
}
```

This is the payload/identity split doing something the fused Linux design can't
state as cleanly: a bind mount is **a new `MountIdentity` sharing an existing
`MountPayload`.** `mount_payload_for(source)` fetches the source's payload (its
`FsOps`/`FsPageBacking`), and `MountIdentity::new_cap(.., source_payload, ..)`
binds the *new* identity to that *same* payload. No backend is constructed; the
two mount points are two identities over one filesystem instance. The new mount's
`root` is the source rnode, so a walker crossing into the target lands directly
in the source subtree (Chapter 5's crossing). `register_mount` keys it under
`(target parent payload, target inode)` in the global table. (v1 is
non-recursive — `rbind` is a follow-up.)

**Remount** (`MS_REMOUNT`) just swaps flags atomically (`mount/mod.rs:974`):

```rust
fn remount(mount, new_flags) { mount.set_flags(new_flags); }
```

A concurrent walker sees either the old or new flags, never a torn value
(`set_flags` is a single atomic store) — the same publish-atomicity discipline
as the rest of the mount layer.

### Mount namespaces

Each process sees the tree through a `MountNamespace` (`mount/mod.rs:524`):

```rust
pub struct MountNamespace { root: Cap<MountIdentity>, mounts: SpinMutex<Vec<MountTableEntry>> }
struct MountTableEntry { parent_payload_ptr: usize, child_fs_object_id: FsObjectId, mount: IdentitySlot<MountIdentity> }
```

The table maps `(parent payload, mountpoint inode) → mount`. The walker consults
*this* table when deciding whether a dentry is a mount point (Chapter 4 stage 7,
Chapter 5 crossing), so two processes with different namespaces resolve the same
path into different filesystems.

```
   Process A                              Process B (after CLONE_NEWNS)
     │ nsproxy.mnt_ns                        │ nsproxy.mnt_ns
     ▼                                       ▼
   MountNamespace A                       MountNamespace B   (clone_ns: copied table)
     mounts: Vec<MountTableEntry>           mounts: Vec<MountTableEntry>
       ├ (rootfs_pp, /dev inode) → devfs      ├ (rootfs_pp, /dev inode) → devfs   ← shared at clone
       ├ (rootfs_pp, /proc inode)→ procfs     ├ (rootfs_pp, /proc inode)→ procfs
       └ (rootfs_pp, /mnt inode) → ext4_A     └ (rootfs_pp, /tmp inode) → tmpfs_B ← A-only / B-only
            │                                      │  diverge after the clone
            ▼ walker stage 7                       ▼
   "/mnt is a mount in A"                  "/mnt is just a dir in B"
```

`clone_ns` copies the `Vec<MountTableEntry>` so the two namespaces start
identical and then diverge — a mount in A's table after the clone is invisible to
B and vice versa. The walker's mount-point test is "is `(current payload, child
inode)` a key in *my* namespace's table?", which is why the same path resolves
differently per process.

`CLONE_NEWNS` (unshare/clone a new mount namespace) is `clone_ns`
(`mount/mod.rs:635`):

```rust
fn clone_ns(&self) -> Result<Cap<Self>, ZoneError> {
    let cloned = self.mounts.lock().iter().cloned().collect();   // copy the table
    runtime::sign(Self { root: self.root.clone(), mounts: SpinMutex::new(cloned) })
}
```

A shallow copy: the child gets its own mount *table* (so later mounts diverge)
but initially shares the same mounts and root. The namespace lives in the
process's `NsProxy` bundle (`process/nsproxy.rs`) alongside pid/net/uts/ipc
namespaces; `setns(2)` resolves `/proc/<pid>/ns/mnt` to a `MountNamespace` cap
and rebuilds the `NsProxy` with it swapped in
(`clone_nsproxy_with_mount_namespace`). The `NsProxy` is immutable after
publication — unshare/setns always build a fresh bundle — which is the process
side of the same "publish a new immutable value rather than mutate in place"
rule.

### The new mount API (`fsopen`/`fsmount`/`open_tree`)

The modern Linux mount API models a mount as a *file* you configure in stages.
`MountApiFile` (`mount/mod.rs:169`) is its `OpenFileBacking::MountApi` object:

```rust
pub enum MountApiFileKind { FsContext, DetachedMount, OpenTree }
pub enum FsContextMode { New, Reconfigure }
```

The staged lifecycle: `fsopen("ext4")` → a `FsContext` (mode `New`); set options
on the fd; `fsmount(ctx, flags)` → a `DetachedMount` (configured but not yet in
any namespace); finally a move-mount attaches it. `fspick` opens an existing
mount as a `FsContext` in `Reconfigure` mode; `open_tree` snapshots a subtree.
It is an alternate front-end producing the same `MountIdentity`/`MountPayload`
the classic `mount(2)` path produces.

### Boot bringup: the first mounts

There is a chicken-and-egg problem: the VFS needs a root filesystem before it
can resolve any path, but mounting normally goes *through* the VFS. Boot
resolves it by constructing the root mount by hand
(`tx-kernel/src/init.rs:557`):

```
mount_rootfs_tmpfs():                              // init.rs:557
  (tmpfs, output) = Tmpfs::new_root()              // Chapter 3: MountOutput
  payload = MountPayload::new_cap(output.fs_ops, output.fs_page_backing, …, fstype="tmpfs")
  root_rnode = materialise RNode(Directory), containing_mount = payload.downgrade()
  root_dentry = DEntry(InlineName::ROOT, root_rnode)     // empty name → renders as "/"
  mount = MountIdentity::new_cap(MountId(1), mountpoint=None, root_rnode, parent=None, payload)
  publish MountNamespace on the init process; store ROOT_MOUNT
```

Then the standard mounts stack on top, now using the normal path because a root
exists: `mount_devfs_at_dev` (`init.rs:636`) does a boot-time `mkdir("/dev")` on
the rootfs and mounts devfs there (MountId 2, parent = root); then procfs at
`/proc` (`init.rs:755`), tmpfs at `/dev/shm`, and an ext4 from the `vda` block
device at its mountpoint.

If an **initramfs** is supplied, its contents are unpacked into the rootfs
before pivoting (`tx-fs/src/initramfs.rs:88`):

```rust
fn unpack_initramfs(initrd, root_dentry, mount_payload, guard) -> Result<u64, Errno>
```

It parses the **cpio newc** format (magic `070701`, 110-byte ASCII headers,
`TRAILER!!!` sentinel) and replays each entry through kernel-side VFS helpers —
`kernel_mkdir` for directories, `kernel_create` + write for regular files,
`kernel_symlink` for links — building the initial userspace tree (`/init`, the
busybox binaries) entirely in tmpfs. This is the bridge from "a blob the
bootloader handed us" to "a filesystem the walker can resolve."

> **Traditional VFS vs txKernel.** Linux builds its first root the same way in
> spirit: `init_mount_tree` constructs the rootfs mount before userspace, and
> `populate_rootfs` unpacks the cpio initramfs into it. txKernel's
> `mount_rootfs_tmpfs` + `unpack_initramfs` are the direct analogues — the
> difference is only that the objects being constructed are the split
> `MountIdentity`/`MountPayload` pair and zone-signed `RNode`/`DEntry` caps
> rather than a `vfsmount`/`super_block`/`dentry`/`inode` quartet.

## Source anchors

- `RNodeWaitPoints`: `crates/tx-subsystems/src/vfs/notification.rs:30`; `VFS_READABLE`/`VFS_WRITABLE` `:26`
- `fire_read_wait`/`fire_write_wait` + wait accessors: `crates/tx-subsystems/src/vfs/structure.rs:808,816,727`
- wait-source adapter: `crates/tx-subsystems/src/vfs/adapter.rs` (`wait_routing`)
- `PpollOp`: `crates/tx-subsystems/src/vfs/composite.rs:968`
- epoll syscalls (`epoll_wait`, `collect_ready_events`, `ready_mask_for_entry`): `crates/tx-shims/src/linux_syscall/epoll.rs`
- EINTR (`await_any_wait_source`): `crates/tx-shims/src/linux_syscall/io.rs:430`
- `Propagation`: `crates/tx-subsystems/src/mount/mod.rs:121`; propagation/peer fields `:421`
- `bind_mount` / `remount`: `crates/tx-subsystems/src/mount/mod.rs:808,974`
- `MountNamespace` / `clone_ns`: `crates/tx-subsystems/src/mount/mod.rs:524,635`
- `MountApiFile` / `MountApiFileKind`: `crates/tx-subsystems/src/mount/mod.rs:169,149`
- `NsProxy` / `clone_nsproxy_with_mount_namespace`: `crates/tx-subsystems/src/process/nsproxy.rs`
- boot mounts: `crates/tx-kernel/src/init.rs:557,636,755`
- initramfs cpio unpack: `crates/tx-fs/src/initramfs.rs:88`
- Specs: `docs/design/05_filesystem/MOUNT_v1.md`, `docs/Txv3/03_STEP_MODEL_v2.md` §5 (`YieldShape::OnEdge`)
