# Part 5 — Mount: the composite filesystem

Mounting is what makes `/` and `/proc` and `/tmp` parts of one tree while being
different filesystems. It is also where the payload/identity split stops being
an optimisation and becomes a *feature you could not build cleanly any other
way*: forced unmount of a busy filesystem. This chapter is the headline payoff
promised in Chapter 0.

## What a mount is

A mount binds a filesystem's root onto a directory in an existing tree. After
`mount(tmpfs, "/tmp")`, a walk reaching the `DEntry` for `/tmp` crosses into the
tmpfs root and continues there. Traditionally this needs two objects:

- a **`super_block`** — the filesystem instance: its backing device, its
  operations, its inode cache;
- a **`vfsmount`** — the placement: where in the tree this filesystem is
  attached, and the namespace/propagation it belongs to.

txKernel keeps both roles but draws the line differently — and the line it draws
*is* the identity/payload split.

## The split, made structural: `MountIdentity` + `MountPayload`

These are two **separately zone-allocated** objects.

```rust
pub struct MountIdentity {                    // the identity half
    id: MountId,
    mountpoint: Option<Cap<DEntry>>,          // where it's attached
    root: Cap<RNode>,                         // the mounted fs's root inode
    parent: Option<Cap<MountIdentity>>,       // parent mount in the tree
    payload: PayloadBinding<MountPayload>,    // the link to the payload half
    flags: AtomicU64,
    propagation: AtomicU64,                   // shared/private/slave (mount propagation)
    peer_group: AtomicU64,
}

pub struct MountPayload {                     // the payload half
    payload_pin_count: AtomicU32,
    pub fs_ops: Arc<dyn FsOps>,               // the namespace operations
    pub fs_page_backing: Arc<dyn FsPageBacking>, // the page-cache operations
    pub backing: Option<Arc<dyn BlockDevice>>,// the block device (None for tmpfs/procfs)
    pub dev_id: DevId,
    pub options: MountOptions,
    pub fstype: &'static str,
    pub source_label: SourceLabel,
}
```

The division is exactly the one Chapter 2 predicted:

- **`MountIdentity`** is *where this mount sits*: its id, its mountpoint dentry,
  its root rnode, its parent, its propagation. This is what resolvers reference
  and what the mount tree is built from. Held as `Cap<MountIdentity>`.
- **`MountPayload`** is *the live filesystem*: the `FsOps` and `FsPageBacking`
  trait objects, the block device, the mount options. This is the heavyweight
  operational resource. Pinned by `PayloadCap<MountPayload>`.

`MountIdentity` reaches `MountPayload` through a `PayloadBinding<MountPayload>` —
not a direct `PayloadCap`. The binding is the *detachable* link, and that
detachability is the whole point.

```
  ┌─────────────────────┐                       ┌──────────────────────────┐
  │ MountIdentity        │   PayloadBinding      │ MountPayload             │
  │  (Cap-pinned)        │  ┌────────────────┐   │  (PayloadCap-pinned)     │
  │  id, mountpoint,     │  │ installed /    │   │  fs_ops:  Arc<dyn FsOps> │
  │  root: Cap<RNode> ───┼─▶│ DETACHED       │──▶│  fs_page_backing         │
  │  parent, propagation │  │  .upgrade() →  │   │  backing: BlockDevice    │
  │  payload ────────────┼─▶│  Ok | Err(Dead)│   │  payload_pin_count: u32 ◀┼─┐
  └─────────────────────┘  └────────────────┘   └──────────────────────────┘ │
       ▲                                                                       │
       │ Cap<MountIdentity>                          MountPayloadPin ──────────┘
       │ (held by walkers, mount table)              (acquire: ++count, Drop: --count)
   in-flight resolver
```

Two retention axes, two pin types, one detachable link between them. A resolver
holds a `Cap<MountIdentity>` (identity axis); operations hold a
`MountPayloadPin` that bumps `payload_pin_count` (payload axis); and the
`PayloadBinding` is the wire that unmount cuts. After the cut, `.upgrade()`
returns `Err(Dead)` while the identity slot stays alive for whoever still holds
its `Cap`.

> **Traditional VFS vs txKernel.** Linux's `super_block` fuses the filesystem
> instance and its liveness into one object with `s_active`/`s_count` reference
> counts, and forced unmount (`do_umount` with `MNT_FORCE`) drives elaborate
> bookkeeping to release it while references may still exist. txKernel splits
> the instance (`MountPayload`) from its placement-identity (`MountIdentity`) at
> the *allocation* level, so "the filesystem is gone but something still
> references the mount point" is two objects in two states, not one object with
> a carefully-interpreted refcount.

## Crossing a mount: from Chapter 4

Recall the walker's stage 7. A `DEntry` that is a mount point carries
`mounted: Option<Weak<MountIdentity>>`. When the walker reaches it, it upgrades
that weak hint to a live `Cap<MountIdentity>`, reads the mount's `root()` rnode,
and continues the walk there — now consulting the mounted filesystem's `FsOps`
(reached via the `RNode`'s `containing_mount` weak ref → `MountPayload`). The
mount namespace threaded through the walk decides *which* mounts are visible, so
two processes with different namespaces can see different trees.

The `MountNamespace` holds the mount table — a list of `MountTableEntry`
mapping `(parent payload, child fs_object_id) → MountIdentity` — and
`clone_ns()` copies it for a new namespace (the `CLONE_NEWNS` path). Crossing,
listing (`/proc/mounts` via `MountSnapshot`), and `..`-at-a-mount-root all
consult this table.

## The payoff: forced unmount

Here is the case the split is built for. A process is reading a file on a
mounted filesystem — its walk is in flight, it holds references into the mount —
and `umount -f` (or lazy `MNT_DETACH`) fires. The filesystem's backing device
must be released *now*. But the in-flight resolver must not dereference freed
memory.

In txKernel, unmount **drops the payload binding**. Watch what happens to each
half:

1. The `PayloadBinding<MountPayload>` is detached. The `MountPayload` — and with
   it the `Arc<dyn FsOps>`, the `Arc<dyn FsPageBacking>`, and the
   `Arc<dyn BlockDevice>` — reclaims as soon as the last `MountPayloadPin` and
   `PayloadCap` drop. The device is released. No new operation can acquire the
   payload.

2. The `MountIdentity` does **not** necessarily die. Any resolver still holding
   `Cap<MountIdentity>` keeps the *identity* slot alive. Its attempt to use the
   filesystem goes through:

   ```rust
   pub fn payload_cap(&self) -> Result<PayloadCap<MountPayload>, Dead> {
       self.payload.upgrade()          // ← the binding upgrade
   }
   ```

   After detach, `upgrade()` returns `Err(Dead)`. The resolver gets a clean
   "this mount's payload is gone" and unwinds with an error (`ENODEV`/`ESTALE`)
   — it never touches the freed `FsOps` or block device.

This is Chapter 2's Motivation 2 made mechanical. `payload_cap()` returning
`Result<_, Dead>` *is* the cross-axis independence rule: identity liveness and
payload liveness are separate, and "identity alive, payload `Dead`" is a
first-class, safely-handled state. The `MountPayloadPin` type
(`mount/mod.rs:373`) is the operational evidence — RAII over an atomic counter,
so the payload knows when the last operation has let go:

```rust
pub struct MountPayloadPin { payload: Cap<MountPayload> }

impl MountPayloadPin {
    pub fn acquire(payload: &PayloadCap<MountPayload>) -> Self {
        payload.payload_pin_count.fetch_add(1, Ordering::AcqRel);   // ++ on acquire
        Self { payload: payload.clone().into_cap() }
    }
}
impl Drop for MountPayloadPin {
    fn drop(&mut self) {
        self.payload.payload_pin_count.fetch_sub(1, Ordering::AcqRel);  // -- on drop
    }
}
```

Recall from Chapter 7 that a file's `PageContainer` of `kind` `File` holds a
`MountPayloadPin` — so every cached page of every open file on this mount is one
increment of `payload_pin_count`. The payload cannot finish reclaiming until all
of them drop. `acquire` takes a `&PayloadCap` (you can only pin a payload you
already reached through a live binding), and the count is the bridge between
"someone is mid-operation" and "safe to free the device."

```
  before umount -f:                  after umount -f (resolver still walking):

  Cap<MountIdentity> ──┐             Cap<MountIdentity> ──┐   (resolver still holds this)
                       ▼                                  ▼
                 MountIdentity                      MountIdentity
                       │ payload (installed)              │ payload (DETACHED)
                       ▼                                  ▼
                 MountPayload                       upgrade() → Err(Dead)
                  ├ fs_ops                          (MountPayload reclaimed:
                  ├ fs_page_backing                  device released, no UAF)
                  └ block device
```

The resolver's next `payload_cap()` returns `Dead`; it fails the operation
cleanly. When it finally drops its `Cap<MountIdentity>`, the identity slot
reclaims too. Payload died at unmount; identity died when the last user left.
Different times, different objects — exactly as intended.

> **Traditional VFS vs txKernel.** "Device busy" (`EBUSY` on `umount`) and the
> forced-unmount dance exist in Linux because the filesystem instance cannot be
> cleanly separated from everything pointing at it. txKernel can always release
> the payload; the question "is anyone still pointing at the mount?" is answered
> independently by whether `MountIdentity` still has live `Cap`s, and pointing
> at a dead-payload mount is a handled error rather than a hazard.

## Filesystem operations as composite steps

The mutating filesystem syscalls — `mkdir`, `unlink`, `rename`, `chmod`,
`symlink`, `link`, `truncate`, `stat`, `readlink`, `getdents64` — are each built
as a **composite step op** in `composite.rs`. Every one is the same shape: walk
to the relevant dentry (or parent + name), then call the backend. For example
`UnlinkOp` (`composite.rs:298`) walks to the parent and the target, then calls
`FsOps::unlink(parent_id, name, target_id)`:

| Composite op | Syscall(s) | Backend call |
|---|---|---|
| `MkdirOp` | `mkdirat` | `FsOps::mkdir` |
| `MknodOp` | `mknodat` | `FsOps::create_inode` |
| `UnlinkOp` | `unlinkat` | `FsOps::unlink` |
| `RenameOp` | `renameat2` | `FsOps::rename` |
| `SymlinkOp` | `symlinkat` | `FsOps::symlink` |
| `LinkOp` | `linkat` | `FsOps::link` |
| `ChmodOp` / `ChownOp` | `fchmodat` / `fchownat` | `FsOps::step_chmod` / `step_chown` |
| `TruncateOp` | `truncate`, `ftruncate` | `FsPageBacking::truncate` |
| `StatOp` / `StatxOp` | `fstatat`, `statx` | `FsOps::load_inode_meta` |
| `ReadLinkOp` | `readlinkat` | `FsOps::read_link` |
| `Getdents64Op` | `getdents64` | `FsOps::readdir` (one entry per call) |

These are `StepOp`s: they implement `step(&mut self, ctx) -> StepOutcome`, hold
their partial state (the resolved `target: Option<Cap<DEntry>>`) so they can
resume after a walker yield, and are run by the same `drive` bridge from Chapter
3. The op *is* the operation's saved state, just like the walker's
`WalkingState` and the syscall future itself — one more place where a suspended
operation is a value, not a parked stack.

`UnlinkOp` is the one to keep in mind for the capstone: it removes the name via
`FsOps::unlink`, which (per the Chapter 3 contract) drops the link count but
*does not* free the inode — leaving the payload alive for any open file. That is
the setup for the unlinked-but-open story Chapter 10 traces end to end.

## Source anchors

- `MountPayload`: `crates/tx-subsystems/src/mount/mod.rs:292`
- `MountPayloadPin` (operational evidence; `acquire`/`Drop`): `crates/tx-subsystems/src/mount/mod.rs:373`
- `MountIdentity`: `crates/tx-subsystems/src/mount/mod.rs:413`
- `payload_cap()` → `Result<_, Dead>`: `crates/tx-subsystems/src/mount/mod.rs:481`
- `root()` / `mountpoint()` / `payload_binding()`: `crates/tx-subsystems/src/mount/mod.rs:465,469,477`
- Mount table / namespace (`umount`, `clone_ns`): `crates/tx-subsystems/src/mount/mod.rs:613,635`
- Composite ops: `crates/tx-subsystems/src/vfs/composite.rs:39` (Chmod) … `:787` (Getdents64)
- `UnlinkOp`: `crates/tx-subsystems/src/vfs/composite.rs:298`
- Mount spec: `docs/design/05_filesystem/MOUNT_v1.md`
