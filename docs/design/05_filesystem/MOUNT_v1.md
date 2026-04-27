# Mount — v1

<!-- txdoc:05-FILESYSTEM-MOUNT-V1 -->

## Status

<!-- txdoc:MOUNT-STATUS-1 -->

Draft v1 — architecture closed, pending cross-doc integration
(2026-04-25). The mountpoint-index-as-authoritative-binding model,
the lazy-umount payload-retention discipline, the identity/payload
split, the boundary table with MOUNT-BDY-1..3, and the §12
cross-doc edits are all in final shape.

This document specifies the **mount subsystem**: `MountIdentity` /
`MountPayload` / `MountNamespace` entities, the mount tree and
mountpoint index, the per-mount filesystem-instance hosting via
`FsOps` / `FsPageBacking`, and the step catalog for `mount` /
`umount` / `unshare(CLONE_NEWNS)` / `setns(CLONE_NEWNS)`. Mount
provides path-crossing topology to VFS and filesystem-instance
identity to PageBacked, but owns no DEntries, no RNodes, no
filesystem-internal state.

It is the resolution-topology subsystem. Where VFS owns the DEntry
and RNode graphs and walks paths through them, mount owns the
*edges* between those graphs — which DEntry is covered by which
filesystem instance, in which namespace.

Companion documents:

- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) — Identity/Payload
  split (§8.1.1 applies directly), Cap/PayloadCap/Weak,
  SENTINEL_DEAD.
- [`CONCEPTS_v4.md`](../00_meta-framework/CONCEPTS_v4.md) — authoritative bindings
  vs derived materializations and publication rule.
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — ARCH-5 (publication
  rule), BIF-* (bifurcation), STEP-* (step discipline), SIG-*
  (publication).
- [`LIVENESS_v2.1.md`](../00_meta-framework/archived/LIVENESS_v2.1.md) — archived catalog rows for
  MountIdentity / MountPayload, partial order
  `MountIdentity.namespace ⟂ MountPayload.payload`.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) —
  four-module layout, five-phase discipline, substrate primitives.
- [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md) — walker mount
  crossing (§5.4 trail, §6.1 rule 3, §9.3), `require_mount_point`,
  RootCtx.
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) — Frame's `mount_ns` slot
  (§3, lifted from Phase 2 to v1 by this doc).
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) — §3.8 the
  `umount_port` is the single mount-attached wire.
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — `RawPort` semantics for `umount_port`.
- [`BDEV_FS.md`](./BDEV_FS.md) — block-device pseudo-filesystem;
  source for `mount /dev/<x>` paths.
- [`TX_EXT4_PLAN_v1_2.md`](./TX_EXT4_PLAN_v1_2.md) —
  `MountInitContext`, `MetadataPcFactory`, `MountOutput`, `FsOps`,
  `FsPageBacking` — the FS-driver handshake.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) —
  `PageContainerKind::File { fs: PayloadCap<MountPayload>, ... }` — the
  page-cache back-reference that contributes to MountPayload's pin
  count.
- [`DEVICE.md`](../06_devices/DEVICE.md) — devfs is a `MountPayload`.

### What this document pins

<!-- txdoc:MOUNT-WHAT-THIS-DOCUMENT-PINS-1 -->

- Entity structure for `MountIdentity` / `MountPayload` /
  `MountNamespace`.
- The **mountpoint index as the authoritative crossing binding**,
  with `MountIdentity.mountpoint` / `mnt_ns` as consistency fields.
- The mount tree (parent binding, child-DLL materialization).
- Per-mount `FsOps` / `FsPageBacking` storage on `MountPayload`.
- `dev_id` allocation.
- Step catalog: `step_mount`, `step_mount_bootstrap`,
  `step_umount_lazy`, `step_umount_normal`, `step_unshare_mnt_ns`,
  `step_setns_mnt`, `clone_mnt_ns`.
- Walker integration: `lookup_mount_at`, `is_mount_root`,
  `is_mountpoint_in`, `synthesize_dotdot_cross`,
  `require_mount_point`.
- Lazy umount semantics: detach from namespace immediately, retain
  payload until pin count drops.
- Mount flags: `RDONLY` / `NOSUID` / `NODEV` / `NOEXEC` / `NOATIME`,
  with two-source read-only check.
- Boot sequence: `step_mount_bootstrap` for root; subsequent
  `step_mount` for /dev, /proc, /sys, /dev/pts.
- v1 alignment: single root `MountNamespace`; `unshare(CLONE_NEWNS)`
  and `setns` accepted as ABI scaffolding.
- Subsystem-boundary table and three boundary invariants
  (MOUNT-BDY-1..3).

### Zone-derived type policy

<!-- txdoc:MOUNT-ZONE-DERIVED-TYPE-POLICY-1 -->

MOUNT uses policy-based zones for mount identities, payloads, and namespaces;
operation code sees only role-derived evidence:

| Mount declaration | Zone-derived public type | Reclamation role |
|---|---|---|
| `MountIdentity` | `Cap<MountIdentity>`, `Weak<MountIdentity>`, `IdentRef<'g, MountIdentity>` | addressability shell retained by path walkers, Frame slots, and mount indexes |
| `MountPayload` | `PayloadCap<MountPayload>` / `MountPayloadPin` reached through identity | filesystem-instance operational state; retained across lazy umount |
| `MountNamespace` | `Cap<MountNamespace>`, `IdentRef<'g, MountNamespace>` | namespace root and mountpoint-index owner |
| `mountpoint_index` rows | `Cap<MountIdentity>` | authoritative crossing binding |
| child/all-mount lists | derived rows justified by identity fields | EBR-observed projection/materialization, not independent entities |
| `FsOps` / `FsPageBacking` objects | `&'static` trait objects on payload | backend static facts, no zone |

No mount step chooses `Zone<T, RcPolicy>` or `Zone<T, EbrPolicy>`. The hidden
zone policy is fixed by the entity-zone declaration; mount steps reserve, sign,
upgrade, and publish role-shaped evidence.

### What this document defers

<!-- txdoc:MOUNT-WHAT-THIS-DOCUMENT-DEFERS-1 -->

- **`step_pivot_root` / `pivot_root(2)`.** Topology shuffle requires
  observer-safe linearization across four bindings (root_mount, two
  parent edges, two index entries); deferred for a separate spec
  pass. v1 returns ENOSYS.
- **`MNT_FORCE` / force-umount.** Active payload invalidation while
  pins still exist; in-flight semantics subtle; deferred.
- **`MS_REMOUNT`.** Mid-flight option mutation; needs published-slot
  for options; deferred.
- **`MS_SHARED` / `MS_SLAVE` / `MS_PRIVATE` propagation.** Linux
  2.6.15 shared-subtree; out of v1.
- **Bind mounts.** Same MountPayload, multiple MountIdentities;
  trivial extension on the v1 entity factoring; deferred per
  MOUNT-11.
- **`MS_MOVE` / `move_mount`.** Re-parent a mount; needs Phase 2
  parent-binding mutation discipline.
- **Real `unshare(CLONE_NEWNS)`** with COW namespace clone. v1 is
  ABI-only.
- **Real `setns(CLONE_NEWNS)`** crossing into a different namespace.
  v1 is ABI-only.
- **`open_tree` / `fsopen` / `fsmount` / `mount_setattr`.** Linux
  5.2 new mount API.
- **OverlayFS / autofs / unionfs.** Filesystem-specific; out of v1.
- **fsnotify on mount events.** A consumer of `umount_port` is
  sketched but not specified.

---

## 1. Position in the architecture

<!-- txdoc:MOUNT-POSITION-ARCHITECTURE-1 -->

A mount is the **glue** between a path and a filesystem instance. The
`MountPayload` is the per-instance bundle of `FsOps` +
`FsPageBacking` + backing-device handle + frozen options + dev_id;
the `MountIdentity` is the addressability shell that path
resolution and process state hold references to.

The mount subsystem is **not** the path walker. VFS owns walking.
Mount supplies the walker with three pure, guard-scoped queries
(`lookup_mount_at`, `is_mount_root`, `synthesize_dotdot_cross`) and
exposes `MountIdentity.flags` on the witness for downstream
flag-enforcement predicates. Mount is also **not** a filesystem
backend. FS backends (tx-ext4, tmpfs, procfs, devfs, devpts,
bdev-fs) implement `FsOps` / `FsPageBacking`; mount stores their
trait objects on `MountPayload` and dispatches to them via
PageBacked and VFS.

### 1.1 Layering

<!-- txdoc:MOUNT-LAYERING-1 -->

```
mount (this document)              ← mount tree, mountpoint index, lifecycle
    ↓ uses                         ← (consumes from below)
VFS                                ← DEntry/RNode witnesses, walker integration
    ↓ uses
substrate (zone, index, bus)       ← AtomicSlot, PersistentBTree, RawPort

mount provides (consumed by above):
    ↓ to VFS walker                ← lookup_mount_at, synthesize_dotdot_cross
    ↓ to PageBacked                ← Cap<MountPayload> as fs identity
    ↓ to Process (Frame)           ← Cap<MountNamespace>
    ↓ to procfs                    ← all_mounts iterator
    ↓ to Exec                      ← MountIdentity.flags via VFS witness
    ↓ to Boot                      ← step_mount_bootstrap, step_mount

mount delegates to FS backend:
    ← TX_EXT4_PLAN.FsOps           ← namespace ops on the fs instance
    ← TX_EXT4_PLAN.FsPageBacking   ← page fetch/flush
    ← BDEV_FS.BlockDevice          ← raw block I/O (via FS backend, not mount)
```

### 1.2 What mount is, what mount is not

<!-- txdoc:MOUNT-WHAT-MOUNT-WHAT-MOUNT-NOT-1 -->

A mount **is**:

- A **resolution-topology subsystem.** The mount tree determines
  which filesystem instance is reached by a given path in a given
  namespace.
- A **filesystem-instance host.** Each MountPayload pins one FS
  instance via `Arc<dyn FsOps>` + `Arc<dyn FsPageBacking>`.
- A **dev_id allocator.** `stat.st_dev` distinguishes mount
  boundaries; mount issues stable `dev_id` values per mount-payload
  lifetime.
- A **flag publisher.** Per-mount RDONLY/NOSUID/NODEV/NOEXEC are
  read by VFS and Exec via the witness's `mount` field.
- A **detach publisher.** `umount_port` fires when a mount detaches
  from its namespace; the wire is a hint, not authority.

A mount is **not**:

- A path walker (VFS).
- An open-file owner (VFS).
- A filesystem implementation (FS backend).
- A page-cache manager (PageBacked).
- A block-device driver (Device).
- A process/cred holder (Process / Cred).

### 1.3 Why MountIdentity / MountPayload split

<!-- txdoc:MOUNT-WHY-MOUNTIDENTITY-MOUNTPAYLOAD-SPLIT-1 -->

The Identity/Payload split per `object_model §8.1.1` is the load-
bearing structural decision. The justification:

A mount has two independent lifetimes. The **namespace
reachability** lifetime ends when the mount is detached from
`mountpoint_index` — a topology fact. The **filesystem-instance**
lifetime ends when no consumer (open file, page-cache PC, cwd/root
cursor) holds operational evidence — a usage fact. Linux's
single-struct mount (`struct mount` containing both flags and
super_block pointer) carries the full weight through the
detached-but-held window; txKernel splits identity from payload to
let each reclaim independently.

The partial order from `LIVENESS_v2_1` §3:

```
MountIdentity.namespace ⟂ MountPayload.payload
```

— **independent** projections. Lazy umount lowers
`MountIdentity.namespace` while leaving `MountPayload.payload` true
for in-flight users. Force umount (deferred) would lower
`MountPayload.payload` eagerly while `MountIdentity` persists.

---

## 2. Entities

<!-- txdoc:MOUNT-ENTITIES-1 -->

Three entity types. `MountIdentity` and `MountPayload` are split
per `object_model §8.1.1`; `MountNamespace` is co-located.

### 2.1 `MountIdentity`

<!-- txdoc:MOUNT-MOUNTIDENTITY-1 -->

```rust
pub struct MountIdentity {
    pub meta: SlotMeta,

    // Parent edge — explicit three-state enum encoding root /
    // attached / detached (see MountParent below). AtomicSlot allows
    // atomic transitions: <init> -> Root | Attached(...) at sign;
    // Attached(...) -> Detached at lazy umount commit;
    // Phase 2 pivot_root will use expected-old swap to relink.
    pub parent: AtomicSlot<MountParent>,

    // Consistency fields for re-validating mountpoint_index reads.
    // Operational obligation on the covered DEntry; must outlive
    // the mount. (For the root mount: a self-reference sentinel —
    // see §7.3.)
    pub mountpoint: Cap<DEntry>,
    // Operational obligation on this filesystem's root DEntry; the
    // walker substitutes cursor to this on a successful crossing.
    pub root_dentry: Cap<DEntry>,

    // Namespace membership — explicit obligation.
    pub mnt_ns: Binding<MountNamespace, Addressability>,

    // Materialization linkages.
    pub children: DllContainer<MountIdentity>,
    pub child_chain: DllNode<MountIdentity>,    // linked in parent.children

    // Per-mount flags. AtomicU32 because Phase 2 MS_REMOUNT may
    // mutate; v1 sets once at mount.
    pub flags: AtomicU32,

    // Phase 2 propagation. v1: always Private (= 0).
    pub propagation: AtomicU8,

    // Payload attachment.
    pub payload: PayloadBinding<MountPayload>,

    // Publication: umount detachment hint. RawPort because edge-
    // triggered (see SIGNAL_ATTACHMENTS §3.8).
    pub umount_port: RawPort<UmountEvent>,
}

pub enum UmountEvent {
    Detached,
}

/// Three-state parent encoding. Avoids the ambiguity of
/// `Option<Option<Binding<...>>>` and gives `..` traversal a clean
/// switch on what the parent is.
pub enum MountParent {
    /// This is the namespace root mount. `..` from this mount's
    /// root_dentry stays put (per VFS_CHECKS §6.1 rule 2).
    /// Reserved for the mount installed by `step_mount_bootstrap`.
    Root,

    /// Normal attached child mount. `..` from this mount's
    /// root_dentry crosses to the parent mount and the mountpoint
    /// parent (via `synthesize_dotdot_cross`). The Binding holds
    /// addressability evidence on the parent MountIdentity.
    Attached(Binding<MountIdentity, Addressability>),

    /// The mount was lazy-unmounted and no longer has an upward
    /// namespace edge. The previous parent binding has been
    /// dropped. v1 detached-parent policy on `..` is stated
    /// in §4.3.4 and §9 of this document.
    Detached,
}
```

**Field-by-field justification:**

- `parent: AtomicSlot<MountParent>`. Addressability obligation
  (when in `Attached` state): the parent identity must remain
  resolvable while we hold the binding (for `..` traversal across
  mount boundaries; for `/proc/<pid>/mountinfo`'s parent column).
  AtomicSlot wraps the enum so transitions are observable: `Root`
  or `Attached(_)` at sign time, `Attached(_) -> Detached` at lazy
  umount commit. Phase 2 pivot_root would use expected-old
  `swap_commit` to relink. v1 never relinks.
- `mountpoint: Cap<DEntry>`. Operational obligation: the DEntry
  must have a live identity for `lookup_mount_at` to consistency-
  check against. Covering this DEntry means the walker substitutes
  past it; uncovering (umount) restores its visibility.
- `root_dentry: Cap<DEntry>`. Operational obligation: the
  filesystem's root DEntry, constructed at mount time via
  `vfs::execution::create_orphan_dentry`. The walker substitutes
  cursor to this DEntry on crossing.
- `mnt_ns: Binding<MountNamespace, Addressability>`. Addressability
  obligation: the namespace must remain resolvable while the mount
  exists (for `is_mountpoint_in` viewpoint queries; for
  `/proc/mounts` rendering). Class-1 binding per `BINDING_v1`;
  immutable post-commit in v1, mutable in Phase 2 setns.
- `children: DllContainer<MountIdentity>`. Materialization,
  justified by each child's `parent` value. Used for
  `/proc/<pid>/mountinfo` parent traversal and the "no children"
  check on umount.
- `child_chain: DllNode<MountIdentity>`. Intrusive DLL node linked
  in `parent.children`.
- `flags: AtomicU32`. Per-mount VFS policy
  (RDONLY/NOSUID/NODEV/NOEXEC/NOATIME). Read frequently (hot path
  for write/exec syscalls); written once at mount, possibly again
  at MS_REMOUNT (Phase 2).
- `propagation: AtomicU8`. Reserved for Phase 2 (shared-subtree).
  v1 always 0 (Private).
- `payload: PayloadBinding<MountPayload>`. Per `object_model §3.2`:
  identity holds an Option<PayloadCap<Payload>> via PayloadBinding.
  Some during the mount's namespace-reachable lifetime; in v1 lazy
  umount keeps it Some until the pin count drops (see §6).
- `umount_port: RawPort<UmountEvent>`. Edge-triggered publication.
  Subscribers wake on Detached and re-observe under fresh guard
  (SIG-1).

### 2.2 `MountPayload`

<!-- txdoc:MOUNT-MOUNTPAYLOAD-1 -->

```rust
pub struct MountPayload {
    pub meta: SlotMeta,

    // Compound-payload disjunct counter. Incremented by every
    // operational pin acquirer:
    //   - Cap<MountPayload> in PageContainerKind::File
    //   - OpenPin held through an RNode whose containing mount is this
    //   - Frame.cwd / Frame.fs_context.root if the RNode lives here
    // Decremented on Cap drop. SENTINEL_DEAD CAS attempted when
    // count reaches 0.
    pub payload_pin_count: AtomicU32,

    // FS interface (TX_EXT4_PLAN §3.3, §3.4).
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,

    // Backing — Some for block-backed fs (ext4, etc.), None for
    // synthetic (tmpfs, procfs, devfs, devpts).
    pub backing: Option<Arc<dyn BlockDevice>>,

    // Stable device id for stat.st_dev. Derived from
    // (major, minor) for block-backed mounts; allocated from
    // anon_dev_bitmap for synthetic mounts. Released to the bitmap
    // (if anonymous) when MountPayload reclaims.
    pub dev_id: DevId,

    // Mount options frozen at mount time. MS_REMOUNT (Phase 2)
    // would publish a new MountOptions atomically.
    pub options: MountOptions,

    // Filesystem type tag. Static string; used for /proc/mounts
    // rendering and fs-equality checks at mount-time.
    pub fstype: &'static str,

    // Source label for /proc/mounts rendering. Snapshot of the
    // MountSpec.source argument (resolved BlockPath as a string,
    // or MagicName carried verbatim). See §5.1 for MountSource.
    pub source_label: SourceLabel,
}
```

**Field-by-field justification:**

- `payload_pin_count: AtomicU32`. One disjunct of the compound-
  payload predicate per `object_model §3.3`. Incremented by
  every `MountPayloadPin::acquire` call; decremented by Drop. The
  closed catalog of pin acquirers is in §2.5.2; the projection is
  in §2.4. The other disjunct, `mounted_identity_bindings`, counts
  MountIdentity slots whose `payload: PayloadBinding` points at
  this MountPayload (1 in v1; up to N in Phase 2 bind mounts).
  Both disjuncts must be zero before SENTINEL_DEAD attempts.
- `fs_ops: Arc<dyn FsOps>`. Trait object per TX_EXT4_PLAN §3.3.
  `Arc` not `Box` because Phase 2 bind mounts will share fs_ops
  across multiple MountIdentities; v1's choice anticipates this
  without changing structure.
- `fs_page_backing: Arc<dyn FsPageBacking>`. Trait object per
  TX_EXT4_PLAN §3.4. Held by `PageContainerKind::File`'s
  `Cap<MountPayload>` (transitively); page fetch/flush dispatches
  through this.
- `backing: Option<Arc<dyn BlockDevice>>`. Block-device handle.
  `Arc` because the same physical device may back multiple
  filesystem types in succession (mount/umount cycles); `Arc`
  keeps it alive across handoffs.
- `dev_id: DevId`. 32-bit value; for block-backed mounts derived
  from `(major << 8) | minor`; for synthetic mounts allocated from
  `AtomicBitmap<256>` (anonymous-dev pool). DevId is stable for the
  MountPayload's lifetime.
- `options: MountOptions`. Frozen-at-mount struct; details
  deferred to mount-syscall implementation.
- `fstype: &'static str`. Compile-time string from FS-driver
  registration ("ext4", "tmpfs", "procfs", "devfs", "devpts",
  "bdev"). No allocation.

### 2.3 `MountNamespace`

<!-- txdoc:MOUNT-MOUNTNAMESPACE-1 -->

```rust
pub struct MountNamespace {
    pub meta: SlotMeta,

    // The root mount of this namespace. v1: plain Cap, set at boot,
    // never mutated. Phase 2 (when pivot_root lands) becomes
    // AtomicSlot<Cap<MountIdentity>>.
    pub root_mount: Cap<MountIdentity>,

    // The authoritative crossing binding. Walker probes this at
    // every named-component step. Keyed by DEntry slot id.
    // PersistentBTree because v1 anticipates Phase 2 unshare's
    // O(1) clone via root-pointer copy.
    pub mountpoint_index: PersistentBTree<DEntryKey, Cap<MountIdentity>>,

    // Enumeration materialization for /proc/mounts and /proc/self/
    // mountinfo. Justified by each mount's mnt_ns binding.
    pub all_mounts: DllContainer<MountIdentity>,
}
```

**Co-located, no Identity/Payload split.** Per `object_model §8.1.1`:
no degraded-but-addressable state. When the last process referring
to a MountNamespace exits and the last `Cap<MountNamespace>` drops,
every contained mount is unmounted (cascade) and the namespace
slot reclaims. There is no zombie-namespace state.

In v1 there is exactly one MountNamespace, allocated at boot, never
reclaimed during system lifetime. Phase 2 unshare creates additional
namespaces; each follows the standard reclamation path when its
last Cap drops.

### 2.4 Projections (LIVENESS catalog)

<!-- txdoc:MOUNT-PROJECTIONS-LIVENESS-CATALOG-1 -->

| Entity | Projection | Context | Meaning | Mechanism |
|---|---|---|---|---|
| MountIdentity | structural | — | Retention > 0 | identity retention |
| MountIdentity | namespace | MountNamespace | reachable from mnt_ns root via mount-tree binding chain | reachability through `mountpoint_index` and `parent.children` |
| MountIdentity | flags-readable | — | identity Cap exists | identity retention (alias for structural) |
| MountPayload | structural | — | Retention > 0 | identity retention |
| MountPayload | payload | — | `mounted_identity_bindings > 0` ∨ `payload_pin_count > 0` | payload retention (compound, two disjuncts) |
| MountNamespace | structural | — | Retention > 0 | identity retention |

**`mounted_identity_bindings`** counts the number of `MountIdentity`
slots whose `payload: PayloadBinding<MountPayload>` is `Some` and
points at this MountPayload. In v1 each MountPayload has exactly
one MountIdentity (the mount that allocated it), so the count is
either 0 or 1. Phase 2 bind mounts let the count exceed 1: a single
MountPayload backs multiple MountIdentities, each with its own
PayloadBinding. Phrasing the projection as a count rather than a
boolean future-proofs the LIVENESS row without rewriting it.

**Partial order:**

```
MountIdentity:
    structural ⇐ namespace
        (mountpoint_index entry carries Cap<MountIdentity>)

MountPayload:
    structural ⇐ payload
        (any payload pin holds MountPayload alive trivially)

MountIdentity.namespace ⟂ MountPayload.payload
    (lazy umount: identity detached, payload pinned by readers;
     force umount [Phase 2]: identity addressable, payload dropped)
```

The orthogonality is the load-bearing property: it lets lazy umount
do the right thing under concurrent open files.

### 2.5 Closed payload-pin accounting

<!-- txdoc:MOUNT-CLOSED-PAYLOAD-PIN-ACCOUNTING-1 -->

Lazy umount works correctly only if every consumer of a mounted
filesystem holds operational evidence that contributes to
`MountPayload.payload`. A single missed contributor — a directory
fd that doesn't pin, a synthetic procfs RNode that doesn't count —
would let `step_umount_normal` succeed while a real consumer is
still using the mount, or let MountPayload reclaim while a stale
consumer still holds a Cap into it.

The fix is a typed contribution and a closed catalog of acquirers.

#### 2.5.1 The `MountPayloadPin` type

<!-- txdoc:MOUNT-THE-MOUNTPAYLOADPIN-TYPE-1 -->

`MountPayloadPin` is a typed operational contribution per
`object_model §3.3`. Each construction increments
`MountPayload.payload_pin_count`; each drop decrements:

```rust
pub struct MountPayloadPin {
    payload: Cap<MountPayload>,
    // Construction: increments payload_pin_count via epoch-guarded
    // CAS with a payload-projection pre-check (rejects if
    // SENTINEL_DEAD already committed).
    // Drop: decrements payload_pin_count; if it reaches zero and
    // mounted_identity_bindings is also zero, attempts SENTINEL_DEAD.
}

impl MountPayloadPin {
    pub fn acquire(payload: &Cap<MountPayload>, g: &Guard)
        -> Result<MountPayloadPin, Errno>;
    pub fn payload(&self) -> &Cap<MountPayload>;
}
```

Acquisition is fallible: if the MountPayload has reached
SENTINEL_DEAD, `acquire` returns ESTALE. v1 acquires almost always
succeed because the only path to SENTINEL_DEAD is via lazy umount
followed by all existing pins dropping; Phase 2 force-umount would
make ESTALE a more common acquire result.

#### 2.5.2 Closed catalog of acquirers

<!-- txdoc:MOUNT-CLOSED-CATALOG-ACQUIRERS-1 -->

Every operational reference to a mounted filesystem instance
holds a `MountPayloadPin`. This catalog is **closed**: any new
consumer added to the kernel that holds a long-lived reference to
filesystem-instance state must add a row here.

| Acquirer | When acquired | When released | Notes |
|---|---|---|---|
| `OpenFile.mount_payload_pin` | open's phase 4 commit, paired with `OpenFile.mount: Cap<MountIdentity>` | last fd holding this OpenFile closes | Applies uniformly to regular files, directory fds, O_PATH fds, /proc/<pid>/fd/N magic-link reopens, and synthetic fds (procfs, sysfs, devfs, devpts) — any RNode reached through path resolution. |
| `FsContext.cwd_mount_payload_pin` | chdir's phase 4 commit, paired with `cwd_mount: Cap<MountIdentity>` | next chdir, or fork (cloned), or process exit | Even when cwd is on a synthetic filesystem (a process whose cwd is in /proc/<pid>/, for instance). |
| `FsContext.root_mount_payload_pin` | chroot's phase 4 commit, paired with `root_mount: Cap<MountIdentity>` | next chroot, or fork (cloned), or process exit | Boot-time pid1 acquires a pin on the root mount's payload. |
| `PageContainerKind::File.fs_payload_pin` | PC construction (in PageBacked's `pc_for_fs_object`); Cap<MountPayload> field stores the pin Cap | PC reclaim (when no PTEs map any of its frames and no CachePin references) | This is the page-cache back-reference per PAGE_BACKED §3.2. |
| `MountIdentity.payload` PayloadBinding (the `mounted_identity_bindings` disjunct) | mount's phase 4 sign | identity Cap drop (last reference) | Counted by the *other* disjunct of `MountPayload.payload`, not by `payload_pin_count`. Listed here for completeness. |

**Synthetic filesystems** (procfs, sysfs, devfs, devpts) are
mounted exactly like persistent filesystems and have a
MountPayload with `fstype = "procfs"` etc. RNodes generated by
their `FsOps::lookup` are reached through path resolution and
their containing OpenFile/FsContext/PageContainer holds a
`MountPayloadPin` on the synthetic mount's payload. This is what
lets `umount /proc` correctly EBUSY when any process has an open
fd in /proc, and why `umount -l /proc` works (lazy) — pins keep
the synthetic filesystem alive until the last consumer releases.

**StructBacked RNodes** (TTY masters/slaves, pipes' anonymous
RNodes, sockets) are different: they're not reached through a
mount tree at all (the `Tty` lives outside any filesystem; pipes
are anonymous). They hold no MountPayloadPin because there's no
mount to pin. The `EntityAtPath.mount` field for these RNodes is
the devfs/devpts mount they were *opened through*, not the mount
that owns them. A devpts pty, for example, holds a pin on devpts;
when devpts is unmounted, the pin keeps devpts's payload alive
until the pty closes.

#### 2.5.3 What `payload_pin_count == 0` means

<!-- txdoc:MOUNT-WHAT-PAYLOAD-PIN-COUNT-0-MEANS-1 -->

`step_umount_normal`'s phase-1 predicate `no_active_payload_users`
checks `payload_pin_count == 0`. Given the closed catalog above,
this predicate is true exactly when:

- No process has any open file (regular, dir, O_PATH, synthetic)
  whose path resolved through this mount.
- No process has cwd or root within this mount.
- No `PageContainerKind::File` references this mount (no live
  page-cache content).

If any of these hold, the count is positive and umount returns
EBUSY. If none hold, normal umount succeeds and payload reclaims
in phase 4 because `mounted_identity_bindings` drops to 0 (the
identity's PayloadBinding clears at identity Cap drop, which
happens shortly after the umount commit since no one holds an
identity Cap either) and `payload_pin_count` is already 0.

**The approximation footnote.** Walker-held IdentRefs are guard-
scoped and do not appear in the count between syscalls. A
syscall in flight that hasn't yet upgraded its IdentRef to a
MountPayloadPin can race with `step_umount_normal`'s phase-1
check; this is a benign race, since the in-flight syscall will
either complete (acquiring a MountPayloadPin that briefly
out-races umount) or fail at upgrade with ESTALE if umount won.
Linux's umount has the same race window with similar resolution.

---

## 3. Authority model (CONCEPTS §8 / ARCH-5)

<!-- txdoc:MOUNT-AUTHORITY-MODEL-CONCEPTS-8-ARCH-5-1 -->

This subsystem's correctness rests on getting the
authoritative-binding-vs-derived-materialization classification
right. The mountpoint index is the authority; everything else is
derived from it.

### 3.1 The authoritative bindings

<!-- txdoc:MOUNT-THE-AUTHORITATIVE-BINDINGS-1 -->

```text
Authoritative bindings:

  MountNamespace.mountpoint_index[DEntryKey] -> Cap<MountIdentity>
      Path-crossing authority. The walker reads this on every named
      component to decide whether to substitute cursor. Visibility
      boundary for both step_mount (install) and step_umount_lazy
      (withdraw). Linearization point of mount lifecycle in the
      namespace.

  MountIdentity.parent: AtomicSlot<MountParent>
      Tree-structural parent edge. Three states: Root (namespace
      root), Attached(binding) (normal child), Detached (post-
      lazy-umount). v1 transitions: <init> -> Root or Attached(...)
      at sign; Attached(...) -> Detached at lazy umount commit.
      Phase 2 (pivot_root) mutates under expected-old discipline.

  MountIdentity.mnt_ns: Binding<MountNamespace, Addressability>
      Namespace membership. Class-1 binding. v1 immutable post-
      commit. Phase 2 (setns into existing namespace) mutates.

Identity-local consistency fields (used by walker re-validation):

  MountIdentity.mountpoint: Cap<DEntry>
      The DEntry covered by this mount. After a mountpoint_index
      lookup returns mount, the walker re-reads
      `mount.mountpoint == queried_dentry` under the same epoch
      guard. A umount that withdrew the index entry races the
      lookup; mismatch -> walker treats it as None.

  MountIdentity.root_dentry: Cap<DEntry>
      The DEntry the walker substitutes cursor to on a successful
      crossing. Constructed at mount commit via VFS's orphan-dentry
      helper.

Derived materializations (justified by the bindings above):

  MountIdentity.parent.children DLL entry
      Justified by child.parent value. Probed for /proc/<pid>/
      mountinfo parent traversal and for "no children" check on
      umount.

  MountNamespace.all_mounts DLL entry
      Justified by child.mnt_ns value. Probed for /proc/mounts
      and /proc/<pid>/mountinfo enumeration.
```

### 3.2 Why the index is authoritative, not derived

<!-- txdoc:MOUNT-WHY-INDEX-AUTHORITATIVE-NOT-DERIVED-1 -->

A reasonable mistake — and an earlier draft of this design made it
— is to treat `mountpoint_index` as a materialization "justified by"
`MountIdentity.mountpoint`. Per CONCEPTS §8, that's backwards:

The walker decides whether to cross by reading `mountpoint_index`.
`step_mount`'s linearization point is `index::install_if_absent` on
this BTree. `step_umount_lazy`'s visibility boundary is
`index::withdraw_commit` on this BTree. **The structure that gates
publication is the authoritative binding.** `MountIdentity.mountpoint`
and `mnt_ns` are then the consistency fields that re-validate index
reads — necessary to defend against a successful BTree read that
races a withdrawal, but not themselves authoritative.

This matches VM_v1_2's recipes-vs-pmap layering: recipes (the
authoritative binding, a PersistentBTree) are read by the fault
handler; PTEs are the derived materialization. Mount's
mountpoint_index plays the recipes role; the per-mount consistency
fields play the role of "this VmEntry is the one I expected."

### 3.3 Walker re-validation in detail

<!-- txdoc:MOUNT-WALKER-RE-VALIDATION-DETAIL-1 -->

```rust
// mount::checks/lookup.rs (sketch)
pub fn lookup_mount_at<'g>(
    mountpoint: IdentRef<'g, DEntry>,
    mnt_ns:     IdentRef<'g, MountNamespace>,
    g:          &'g Guard,
) -> Option<MountTraversal<'g>> {
    let key = DEntryKey::from(mountpoint);

    // 1. BTree probe.
    let mount_cap = mnt_ns.mountpoint_index.get(key, g)?;

    // 2. Observe the looked-up MountIdentity under the same guard.
    let mount = IdentRef::observe(&mount_cap, g);

    // 3. Consistency check — guards against retargeting, NOT against
    //    all stale observation. See the explanation below.
    if !mount.mountpoint.identity_eq(mountpoint) {
        return None;
    }
    if !mount.mnt_ns.observe(g).identity_eq(mnt_ns) {
        return None;
    }

    // 4. Construct the traversal value.
    let root = IdentRef::observe(&mount.root_dentry, g);
    Some(MountTraversal { mount, root_dentry: root })
}

pub struct MountTraversal<'g> {
    pub mount:       IdentRef<'g, MountIdentity>,
    pub root_dentry: IdentRef<'g, DEntry>,
}
```

**What the consistency check does and does not do.** The check
prevents *retargeting*: if a BTree slot were ever reused for a
different DEntry (it is not — DEntry slot ids are stable for the
DEntry's lifetime per object_model §6, but defense-in-depth) or if
the namespace context were ever wrong (it cannot be — the BTree is
per-MountNamespace, but again defense-in-depth), the walker would
catch it. The check is structurally redundant in v1 but documents
the intended semantics and survives Phase 2 changes (bind mounts
sharing payload across MountIdentities, pivot_root mutating
parent edges).

**What it does *not* do** is detect that an umount has already
withdrawn the index entry. After umount commits its
`index::withdraw_commit`, the MountIdentity's `mountpoint` field
remains set (it's a Cap, not removed until the identity slot
reclaims) and `mnt_ns` similarly. A walker that read the BTree
*before* withdrawal and runs the consistency check *after*
withdrawal will see both checks pass and return Some.

This is fine — and load-bearing for the race-degradation theorem.
Per LIVENESS §2.5: a racing umount can cause `lookup_mount_at` to
return:

- **`Some(traversal)`** (we observed pre-state through the BTree).
  The walker crosses into a `MountIdentity` that was alive at the
  moment of observation. The mount may now be detached, but the
  walker holds an IdentRef and can proceed; subsequent operations
  inside the mount succeed if MountPayload pins exist (lazy umount)
  or fail with ESTALE if not (force umount, Phase 2).
- **`None`** (we observed post-state). The walker proceeds into
  the uncovered directory.

The two outcomes are both correct. What is excluded is a third
outcome: silent retargeting (`Some(traversal_to_different_mount)`).
The consistency check is what excludes that — not the timing
relationship between BTree read and umount commit, but the field
match on the observed MountIdentity itself.

### 3.4 Step-mount commit ordering

<!-- txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1 -->

Phase 4 of `step_mount` realizes the authority model in code. The
order matters:

```text
Phase 4 commit (step_mount):

  1. zone::sign(payload_slot)
       -> Cap<MountPayload>, then wrap as PayloadCap<MountPayload>
       The MountPayload now exists structurally (Cap retained by
       the unsigned identity slot's payload field).

  2. zone::sign(identity_slot)
       -> Cap<MountIdentity>
       The MountIdentity now exists structurally with all
       consistency fields (parent, mnt_ns, mountpoint, root_dentry,
       flags) set.

  3. index::install_if_absent(
         mnt_ns.mountpoint_index, dentry_key, identity_cap)
       *** LINEARIZATION POINT ***
       Authoritative crossing binding installed. New walker probes
       observe the crossing from this commit point onward.
       Failure (BTree slot already occupied — concurrent stack
       mount) -> drop reservations, return EBUSY.

  4. index::install_if_match(parent.children, ...)
       Derived materialization. Justified by step 2's
       child.parent value. Infallible (DLL link is wait-free per
       MUTATION_COMPOSITIONS_v1).

  5. index::install_if_match(mnt_ns.all_mounts, ...)
       Derived materialization. Justified by step 2's child.mnt_ns
       value. Infallible.
```

Steps 4 and 5 publish materializations *after* the authoritative
binding (step 3) is in place. This is the publication rule
(CONCEPTS §8.4): every materialization commit re-validates its
justifying binding. In step 4, the justifying binding is
`child.parent`'s value, set in step 2. The DLL link is conditioned
on "the child still has me as parent," which is trivially true at
this point in the same step.

#### 3.4.1 Prepare-time helpers must not publish

<!-- txdoc:MOUNT-PREPARE-TIME-HELPERS-MUST-NOT-PUBLISH-1 -->

A subtle but load-bearing rule for `step_mount`'s phase 3:

> **MOUNT-UNPUBLISHED.** Every VFS object created or mutated by
> `step_mount`'s phase-3 helpers is unpublished until phase 4
> step 3 (`index::install_if_absent` on `mountpoint_index`)
> succeeds. Failure before that linearization point leaves no
> externally reachable path fact and no entry in any mount table.

This rule binds three concrete helper invocations:

- **`vfs::execution::create_orphan_dentry`** returns a DEntry
  reservation whose `parent` is None. The DEntry is reachable only
  through the in-flight step's local frame; no published DEntry
  has it in `children`. Drop on failure reclaims the slot.

- **`vfs::execution::find_or_create_rnode`** may return an existing
  RNode from the per-MountPayload coherence index, or it may
  insert a new one. New RNode insertions are scoped to the
  unsigned MountPayload's coherence index; since no Cap<MountPayload>
  yet exists outside this step, no other subsystem can observe the
  new RNode. On failure, the find_or_create's reservation Drop
  removes the entry from the per-MountPayload coherence index, and
  the unsigned MountPayload's eventual reservation Drop tears down
  the index entirely.

- **`vfs::execution::attach_rnode_to_dentry`** mutates only the
  unpublished `root_dentry`. No `children` container of any
  published DEntry is touched.

The invariant the rule preserves:

> **Failure before `mountpoint_index` install leaves no externally
> reachable path fact and no entry in any mount table.**

This keeps `mountpoint_index` as the **sole visibility boundary**
for the new mount. A failed mount is observationally identical to
no-mount-attempted from outside the step. (Compare with
`step_pivot_root` in Phase 2, which does not have this property —
it shuffles already-published topology, so its visibility
boundary is the slot-locked-with-re-read primitive over multiple
already-live bindings.)

### 3.5 Umount commit ordering (lazy)

<!-- txdoc:MOUNT-UMOUNT-COMMIT-ORDERING-LAZY-1 -->

```text
Phase 4 commit (step_umount_lazy):

  1. index::withdraw_commit(
         mnt_ns.mountpoint_index, dentry_key, expected: identity_cap)
       *** VISIBILITY BOUNDARY 1 ***
       Authoritative crossing binding withdrawn. New walker
       probes return None and walk into the uncovered directory.

  2. index::withdraw_commit(parent.children, ...)
       Derived materialization withdrawn. /proc/<pid>/mountinfo no
       longer enumerates this mount as a child of its parent.

  3. index::withdraw_commit(mnt_ns.all_mounts, ...)
       Derived materialization withdrawn. /proc/mounts no longer
       lists this mount.

  4. parent.swap(MountParent::Detached)
       AtomicSlot mutation: Attached(parent_binding) -> Detached.
       The previous Attached value's Drop releases the parent
       MountIdentity Cap. Detached is a terminal state in v1; Phase 2
       pivot_root will introduce Detached -> Attached(new_parent) via
       expected-old swap.

  5. (DO NOT touch identity.payload.)
       MountIdentity.payload remains Some. MountPayload stays
       alive. payload_pin_count holders (open files, page-cache
       PCs, cwd/root cursors) continue using the payload normally.
       When the last pin drops AND identity.payload is finally
       cleared (which will happen when the last Cap<MountIdentity>
       drops), MountPayload becomes eligible for SENTINEL_DEAD and
       fs_ops::shutdown runs in the reclaim queue.
```

Note step 5: lazy umount does **not** drop the PayloadBinding. That
distinguishes it from force-umount (deferred). The MountPayload's
liveness is governed entirely by `payload_pin_count` plus the
identity-payload binding; the binding survives the namespace
detach.

### 3.6 The race-degradation walkthrough

<!-- txdoc:MOUNT-THE-RACE-DEGRADATION-WALKTHROUGH-1 -->

A worked example — concurrent walker and umount — to verify the
race-degradation theorem:

```text
T=0: walker has cursor at /a, about to traverse /a/b.
     Mount M is attached at /a/b in the current namespace.

T=1: umount M starts on another core.

[interleaving 1 — walker before umount]
T=2: walker calls lookup_mount_at(/a/b, mnt_ns, g).
     BTree probe returns Cap<M>.
     Consistency check: M.mountpoint == /a/b dentry? Yes. M.mnt_ns
     match? Yes. Returns Some(MountTraversal).
     Walker pushes MountBoundary, cursor = M.root_dentry.
T=3: walker continues into M's filesystem.
T=4: umount commit phase 4 step 1 fires. mountpoint_index loses
     /a/b -> M entry.
T=5: walker is now inside M. Its trail has MountBoundary; current_
     mount Cap-upgraded across any intervening yield. Walker
     completes its operation. M.payload.is_some() throughout
     (lazy umount).

[interleaving 2 — umount before walker]
T=2: umount commit phase 4 step 1 fires. mountpoint_index withdraws
     /a/b -> M.
T=3: walker calls lookup_mount_at(/a/b, mnt_ns, g). BTree probe
     returns None.
T=4: walker proceeds without crossing — it walks into /a/b's
     children (the underlying directory, now uncovered).

[interleaving 3 — race in the middle]
T=2: walker calls lookup_mount_at. BTree probe returns Cap<M>
     (saw pre-state).
T=3: umount commit phase 4 step 1 fires.
T=4: walker's consistency check runs. M.mountpoint == /a/b? Yes
     (consistency fields aren't withdrawn until M's identity slot
     reclaims, which is gated on Cap drop). Returns
     Some(MountTraversal).
T=5: walker pushes MountBoundary, crosses, continues.
     M is in the "detached but held" state — namespace projection
     false, payload projection true. Walker's operations succeed
     against M's payload via existing pins.
T=6: walker eventually finishes; current_mount IdentRef released.
     If walker held no operational evidence on M.payload (it just
     traversed through), nothing pins M; eventually identity Cap
     refcount drops, MountIdentity reclaims, then MountPayload
     reclaims.

All three interleavings produce correct, race-free outcomes:
walker either crosses M (and operates on a real if detached mount)
or walks into the uncovered directory. No silent retarget; no
operation against a different entity than the path describes.
```

This is the LIVENESS §2.5 race-degradation theorem made concrete
for mount.

---

## 4. Walker integration

<!-- txdoc:MOUNT-WALKER-INTEGRATION-1 -->

The mount subsystem's most-consulted interface is the path walker.
Mount provides four guard-scoped functions; VFS consumes them at
specific points in `kernel_step`.

### 4.1 The walker-facing API

<!-- txdoc:MOUNT-THE-WALKER-FACING-API-1 -->

```rust
// mount::checks/lookup.rs

/// Probe the mountpoint index for a possible mount crossing.
/// Returns Some if `mountpoint` is mounted-on in `mnt_ns`; the
/// caller substitutes cursor to `traversal.root_dentry` and
/// pushes a MountBoundary trail entry.
pub fn lookup_mount_at<'g>(
    mountpoint: IdentRef<'g, DEntry>,
    mnt_ns:     IdentRef<'g, MountNamespace>,
    g:          &'g Guard,
) -> Option<MountTraversal<'g>>;

/// Test whether `d` is the root dentry of some mount in `mnt_ns`.
/// Used by the walker's `..` rule and by require_mount_point.
pub fn is_mount_root<'g>(
    d:      IdentRef<'g, DEntry>,
    mnt_ns: IdentRef<'g, MountNamespace>,
    g:      &'g Guard,
) -> bool;

/// Test whether `d` is a mountpoint (covered DEntry) in `mnt_ns`.
/// Used by VFS's rmdir/rename refinement wrappers (MOUNT-6).
pub fn is_mountpoint_in<'g>(
    d:      IdentRef<'g, DEntry>,
    mnt_ns: IdentRef<'g, MountNamespace>,
    g:      &'g Guard,
) -> bool;

/// Synthesize a `..`-cross when the walker's trail is empty and
/// cursor is at a mount root. Reads current_mount.parent (which is
/// MountParent::Root, MountParent::Attached(_), or
/// MountParent::Detached) and current_mount.mountpoint to construct
/// the appropriate result.
pub fn synthesize_dotdot_cross<'g>(
    current_mount: IdentRef<'g, MountIdentity>,
    mnt_ns:        IdentRef<'g, MountNamespace>,
    g:             &'g Guard,
) -> DotDotResult<'g>;

pub struct MountTraversal<'g> {
    pub mount:       IdentRef<'g, MountIdentity>,
    pub root_dentry: IdentRef<'g, DEntry>,
}

/// Three-way result corresponding to MountParent's three states.
/// The walker switches on this to decide what to do with `..` at
/// an empty-trail mount-root cursor.
pub enum DotDotResult<'g> {
    /// current_mount is the namespace root. Cursor stays put.
    /// (Walker semantics match VFS_CHECKS §6.1 rule 2 for
    /// chroot/mnt_ns_root: `..` is a no-op.)
    StayAtRoot,

    /// current_mount has a live parent. Cross to parent mount and
    /// mountpoint parent.
    Cross(DotDotCross<'g>),

    /// current_mount is detached (post-MNT_DETACH; parent edge
    /// dropped). v1 policy: walker returns ENOENT/ESTALE for `..`
    /// across this edge. (See §6.6 for the rationale: detached
    /// mounts admit relative walks within them but not upward.)
    DetachedFail,
}

pub struct DotDotCross<'g> {
    pub parent_mount: IdentRef<'g, MountIdentity>,
    pub mountpoint:   IdentRef<'g, DEntry>,
}
```

All four are pure, guard-scoped, side-effect-free. They read mount
structure under guard and return `IdentRef`-bearing values that the
walker consumes inline. None of them take retention; none of them
mutate.

These functions live in `mount::checks/lookup.rs` rather than
`require.rs` because they don't gate operations with errno
selection — they're observation primitives the walker uses to
decide its next move. They follow the same purity discipline as
predicates (PRED-1, PRED-2).

### 4.2 The covering invariant (MOUNT-3 made explicit)

<!-- txdoc:MOUNT-THE-COVERING-INVARIANT-MOUNT-3-MADE-EXPLICIT-1 -->

> **MOUNT-COVER.** When the walker is resolving component X under
> parent directory P:
>
> 1. The walker resolves `P.children[X]` to child DEntry D (cache
>    lookup, possibly with NeedIO trampoline).
> 2. **Before D is used as the directory for the next component's
>    lookup**, the walker calls `mount::checks::lookup_mount_at(D,
>    mnt_ns, g)`.
> 3. If a mount is found, the walker pushes
>    `TrailEntry::MountBoundary { was_at: D }` and substitutes
>    `cursor = mount.root_dentry`.
> 4. While `(D's DEntryKey, mount) ∈ mountpoint_index[mnt_ns]`,
>    the walker does not consult D's children container for forward
>    traversal.

The covered DEntry is held alive by `mount.mountpoint: Cap<DEntry>`.
Its children container persists, ready to serve lookups again post-
umount. Mount does not modify the mountpoint DEntry beyond holding
this Cap. (MOUNT-3 satisfied.)

### 4.3 Walker integration points in VFS_CHECKS

<!-- txdoc:MOUNT-WALKER-INTEGRATION-POINTS-VFS-CHECKS-1 -->

Five concrete integration points with [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md);
implementation should keep these in sync with §12.

#### 4.3.1 `WalkState` gains `current_mount` (VFS_CHECKS §5.1)

<!-- txdoc:MOUNT-WALKSTATE-GAINS-CURRENT-MOUNT-VFS-CHECKS-5-1-1 -->

```rust
pub struct WalkState<'g> {
    pub cursor: IdentRef<'g, DEntry>,
    pub remaining: Components<'a>,
    pub symlink_budget: u8,
    pub root_ctx: RootCtxRef<'g>,
    pub trail: WalkTrail<'g>,

    // NEW: tracks which mount the cursor currently lives in.
    // Initialized from root_ctx.mnt_ns.root_mount; updated on
    // MountBoundary push/pop.
    pub current_mount: IdentRef<'g, MountIdentity>,
}
```

`current_mount` is observation, not retention — it's an IdentRef.
On MountBoundary push (forward crossing), the walker stashes the
old `current_mount` in the trail entry and updates the field. On
MountBoundary pop (`..`), the walker restores from the trail.

#### 4.3.2 `RootCtxRef` gains `mnt_ns` (VFS_CHECKS §5.5)

<!-- txdoc:MOUNT-ROOTCTXREF-GAINS-MNT-NS-VFS-CHECKS-5-5-1 -->

```rust
pub struct RootCtxRef<'g> {
    pub mnt_ns:      IdentRef<'g, MountNamespace>,    // NEW
    pub mnt_ns_root: IdentRef<'g, DEntry>,            // = mnt_ns.root_mount.root_dentry
    pub chroot:      Option<IdentRef<'g, DEntry>>,
    pub cwd:         IdentRef<'g, DEntry>,
}
```

Derived from `Frame.mount_ns: Cap<MountNamespace>` at walk start.
The walker passes it to mount's check functions on every probe.

#### 4.3.3 Forward crossing in `kernel_step` rule 3 (VFS_CHECKS §6.1)

<!-- txdoc:MOUNT-FORWARD-CROSSING-KERNEL-STEP-RULE-3-VFS-CHECKS-1 -->

```rust
// After resolving D as the looked-up child of P:
//
if let Some(traversal) = mount::checks::lookup_mount_at(
    new_cursor, state.root_ctx.mnt_ns, guard,
) {
    state.trail.push(TrailEntry::MountBoundary {
        was_at: new_cursor,
        was_in_mount: state.current_mount,
    });
    state.cursor = traversal.root_dentry;
    state.current_mount = traversal.mount;
} else {
    state.cursor = new_cursor;
    // current_mount unchanged
}
```

The TrailEntry now carries both the previous DEntry and the
previous mount, so `..` can restore both.

```rust
pub enum TrailEntry<'g> {
    DEntry(IdentRef<'g, DEntry>),
    MountBoundary {
        was_at: IdentRef<'g, DEntry>,
        was_in_mount: IdentRef<'g, MountIdentity>,    // NEW
    },
}
```

#### 4.3.4 `..` in `kernel_step` rule 2 (VFS_CHECKS §6.1)

<!-- txdoc:MOUNT-IN-KERNEL-STEP-RULE-2-VFS-CHECKS-6-1 -->

The existing rule:

```text
"..": pop trail. If TrailEntry::MountBoundary, cursor becomes
was_at. Otherwise cursor becomes popped DEntry. If trail empty
and cursor at chroot or mnt_ns_root, cursor stays.
```

Becomes:

```text
"..": pop trail.
    If TrailEntry::MountBoundary { was_at, was_in_mount }:
        cursor = was_at
        current_mount = was_in_mount
    Else if TrailEntry::DEntry(d):
        cursor = d
    Else (trail empty):
        If cursor at chroot or mnt_ns_root: cursor stays.
        Else if is_mount_root(cursor, mnt_ns):
            // Empty-trail mount-root case (MOUNT-4 gap closure).
            switch synthesize_dotdot_cross(current_mount, mnt_ns, g):
                DotDotResult::StayAtRoot:
                    cursor stays.
                DotDotResult::Cross(cross):
                    cursor = cross.mountpoint.parent (via dcache cache)
                    current_mount = cross.parent_mount
                DotDotResult::DetachedFail:
                    return Error(DetachedNamespace).
                    classify -> ENOENT.
                    (v1 policy: `..` across a detached parent edge
                    fails. See §6.6 for the rationale.)
        Else:
            cursor stays.
```

The three-way switch corresponds directly to MountParent's three
states. Without distinguishing Root from Detached, the walker
would conflate "stay put because there's no parent" (root mount)
with "stay put because parent went away" (detached mount), giving
the wrong semantics for the latter.

The empty-trail mount-root case is the MOUNT-4 gap closure: a
process that did `chdir("/some/mount"); openat(".", "..", ...)` has
no MountBoundary in its trail (the trail is reset between syscalls)
but cursor is at a mount root. Without this case, `..` would
incorrectly stay put, violating POSIX semantics that `..` from a
filesystem root crosses to the parent mount.

#### 4.3.5 `EntityAtPath.mount` field (VFS_CHECKS §11)

<!-- txdoc:MOUNT-ENTITYATPATH-MOUNT-FIELD-VFS-CHECKS-11-1 -->

```rust
pub struct EntityAtPath<'g> {
    pub dentry: IdentRef<'g, DEntry>,
    pub rnode:  IdentRef<'g, RNode>,
    pub mount:  IdentRef<'g, MountIdentity>,    // NEW
}
```

At `build_witness`, the walker copies `state.current_mount` into
the witness. Downstream consumers (stat, exec, write paths) read
the mount's flags via this field for MOUNT-5 (st_dev) and MOUNT-12
(flag enforcement).

#### 4.3.6 `require_mount_point` and refinement wrappers

<!-- txdoc:MOUNT-REQUIRE-MOUNT-POINT-REFINEMENT-WRAPPERS-1 -->

VFS's `require_mount_point` in §10.1 is implemented in
`mount::checks::require_mount_point` and re-exported by VFS:

```rust
// mount::checks/require.rs
pub async fn require_mount_point<'g>(
    path: &[u8],
    ctx:  &ResolveCtx,
) -> Result<MountPointAtPath<'g>, Errno> {
    let entity = vfs::checks::require_entity(path, ctx).await?;
    if !is_mount_root(entity.dentry, ctx.root_ctx.mnt_ns, &guard) {
        return Err(EINVAL);    // not a mount point
    }
    Ok(MountPointAtPath {
        dentry: entity.dentry,
        rnode:  entity.rnode,
        mount:  entity.mount,
    })
}
```

VFS's refinement wrappers `require_rmdirable_dir_child` and
`require_parent_and_named_child` (rename old side) gain the
mountpoint-EBUSY clause (MOUNT-6):

```rust
// vfs::checks/require.rs (extended)
pub async fn require_rmdirable_dir_child<'g>(
    path: &[u8], ctx: &ResolveCtx,
) -> Result<RmdirableDirChild<'g>, Errno> {
    let base = require_parent_and_named_child(path, ctx).await?;
    if !predicates::is_directory(&base.child.child_rnode()) {
        return Err(ENOTDIR);
    }
    if !predicates::is_empty_directory(&base.child).await? {
        return Err(ENOTEMPTY);
    }
    // NEW: mountpoint check.
    if mount::checks::is_mountpoint_in(
        base.child, ctx.root_ctx.mnt_ns, &guard,
    ) {
        return Err(EBUSY);
    }
    Ok(RmdirableDirChild(base))
}
```

Same pattern in `require_parent_and_named_child` for rename's old
side.

### 4.4 Walker hot-path cost

<!-- txdoc:MOUNT-WALKER-HOT-PATH-COST-1 -->

One BTree probe per crossed component. The BTree root is on
`MountNamespace` (one pointer hop from `RootCtxRef`); a typical
mount tree has depth ≤ 4 BTree levels for ~1000 mounts (well above
realistic mount counts on small embedded systems). Each level is a
cache-friendly node read.

For the common case — a path with no mount crossings — the probe
returns None on the first BTree level (the BTree is sparse on
DEntryKey space; most DEntries aren't mountpoints). This is the
hot path: one or two cache-warm reads per component.

---

## 5. Step catalog

<!-- txdoc:MOUNT-STEP-CATALOG-1 -->

Five mutating steps plus one helper. All mutating steps follow the
five-phase discipline (STEP-4): observe, upgrade, reserve, commit,
publish.

### 5.1 `step_mount`

<!-- txdoc:MOUNT-STEP-MOUNT-1 -->

Install a new filesystem instance at a target directory.

```rust
pub fn step_mount(
    spec: MountSpec,
    ctx:  &ThreadContext,
) -> StepOutcome<Cap<MountIdentity>>;

pub struct MountSpec {
    pub source:  MountSource,
    pub target:  Path,                   // mountpoint
    pub fstype:  &'static str,
    pub flags:   MountFlags,
    pub options: MountOptions,           // fs-specific
}

/// The source argument of mount(2) is overloaded across filesystem
/// types: a path for block-backed FSes, a magic name for synthetic
/// FSes, sometimes ignored. The enum makes the cases explicit.
pub enum MountSource {
    /// No source. Some synthetic mounts accept this
    /// (`mount -t tmpfs none /tmp`).
    None,

    /// A path that must resolve to a block-device RNode (in
    /// bdev-fs's mount). Required for block-backed filesystems
    /// (ext4, etc.).
    BlockPath(Path),

    /// A magic name preserved verbatim for /proc/mounts rendering
    /// but ignored for object construction. Conventional values:
    /// "proc", "sysfs", "tmpfs", "devpts", "devfs". The
    /// MountSource is kept on the MountPayload (see source_label
    /// field below) for /proc/mounts and getmntent compatibility.
    MagicName(NameOwned),
}
```

**Source validation by fstype:**

| fstype | Valid `MountSource` |
|---|---|
| `ext4` | `BlockPath(p)` (required) |
| `bdev` | `None` (bootstrap-only, mounted at `/dev/block`) |
| `tmpfs` | `None` or `MagicName("tmpfs"/"none"/...)` |
| `procfs` | `None` or `MagicName("proc"/"none")` |
| `sysfs` | `None` or `MagicName("sysfs"/"none")` |
| `devfs` | `None` or `MagicName("devfs"/"none")` |
| `devpts` | `None` or `MagicName("devpts"/"none")` |

`mount::checks::require_source_for_fstype` validates the
combination at phase 1. Errno: EINVAL on mismatch.

**Source label retention.** `MountPayload` gains a
`source_label: SourceLabel` field for /proc/mounts rendering:

```rust
pub enum SourceLabel {
    /// Block device path resolved at mount time (e.g. "/dev/vda1").
    /// Stored as a string snapshot, not a live path.
    BlockPath(String),
    /// Magic name carried verbatim ("proc", "sysfs", "tmpfs",
    /// or even "none" — userspace mount(8) often passes "none").
    Magic(String),
    /// Empty label; rendered as the empty string in /proc/mounts.
    None,
}
```

This is a small cross-doc edit on §2.2 (`MountPayload` field
list) — captured in §12.

**Phase 1 — observe.**

```text
- cred::checks::require_cap_sys_admin(ctx)
    Returns EPERM if caller lacks CAP_SYS_ADMIN. (MOUNT-9.)
- vfs::checks::require_directory(spec.target, ctx)
    Witness: target dentry must exist and be a directory.
    Returns ENOTDIR / ENOENT as appropriate.
- mount::checks::require_target_not_already_mounted(
      target_dentry, ctx.frame.mount_ns)
    Probes mountpoint_index; returns EBUSY if dentry is already
    mounted-on. (Stack-mount rejection, MOUNT-11 v1 default.)
- If spec.source is Some:
      vfs::checks::require_block_device_source(spec.source, ctx)
      Witness: source path must resolve to a block-device RNode.
      For tmpfs/procfs/devfs spec.source is None.
```

**Phase 2 — upgrade.**

```rust
let target_dentry_cap: Cap<DEntry> = upgrade(witness.target_dentry)?;
let mnt_ns_cap: Cap<MountNamespace> = upgrade(witness.mnt_ns)?;
let parent_mount_cap: Cap<MountIdentity> = upgrade(witness.target_mount)?;
let block_device: Option<Arc<dyn BlockDevice>> = match witness.source {
    Some(s) => Some(extract_block_device(s)?),
    None => None,
};
```

`extract_block_device` reads the source RNode's
`PageContainerKind::File { fs: Cap<MountPayload> /* bdev-fs */, ... }`
and looks up the `BlockDeviceHandle` via bdev-fs's helper (see
BDEV_FS §8). Failure here returns ENOTBLK.

**Phase 3 — reserve.**

```text
- zone::reserve(MountIdentity)
    Reserves an identity slot.
- zone::reserve(MountPayload)
    Reserves a payload slot.
- index::reserve_slot(mnt_ns.mountpoint_index, target_key)
    Conditional on dentry-absent. Failure returns EBUSY.
- dll::reserve_slot(parent_mount.children)
    Reserves a child-DLL link.
- dll::reserve_slot(mnt_ns.all_mounts)
    Reserves an enumeration link.
- dev_id := allocate_dev_id(block_device, anon_dev_bitmap)
    For block-backed: derives from (major, minor).
    For synthetic: reserves from anon_dev_bitmap.
    Returns ENOSPC on bitmap exhaustion (synthetic only).
- root_dentry_cap := vfs::execution::create_orphan_dentry(
      DentryKind::MountRoot,
      /* root_rnode placeholder; filled after fs_driver.mount_init */
  )
    Reserves a DEntry slot via VFS's helper.

- *** FS-driver handshake ***
- ctx_init := MountInitContext {
      block_device: block_device.clone()?,
      mount_id: tentative_mount_id,
      metadata_pc_factory,
      options: spec.options,
  }
- output := fs_driver.mount_init(ctx_init)?
    Fallible: bad superblock, corrupted journal, EROFS-but-RW-asked,
    feature mismatch. Failure drops all reservations cleanly.
    Output: { fs_ops, fs_page_backing, root_fs_object_id, root_inode_meta }
- root_rnode_cap := vfs::execution::find_or_create_rnode(
      output.root_fs_object_id,
      output.root_inode_meta,
      &ctx_init.metadata_pc_factory,
  )
    Constructs the root RNode, wiring it to fs_page_backing.
- vfs::execution::attach_rnode_to_dentry(
      root_dentry_cap, root_rnode_cap)
    Completes the orphan dentry construction.

  *** UNPUBLISHED-STATE INVARIANT ***
  Every VFS object created during phase 3 above is unpublished
  until phase 4 step 3 (mountpoint_index install) succeeds:
    - root_dentry_cap is parentless (parent: None) and reachable
      only through this step's local frame.
    - root_rnode_cap is in VFS's RNode coherence index for this
      MountPayload (find_or_create's published side), but the
      MountPayload itself is still unsigned and has no
      Cap<MountIdentity> referring to it.
    - attach_rnode_to_dentry mutates only the unpublished
      root_dentry; no published DEntry's children container is
      touched.
  Failure between this point and phase 4 step 3 drops every
  reservation. The dropped root_dentry_cap reclaims (no published
  reference). The dropped root_rnode_cap may briefly remain in
  the coherence index until the find_or_create's reservation Drop
  clears it. The dropped MountPayload reservation releases the
  fs_ops Arc, which calls fs_driver-defined teardown (close
  superblock, drop journal handle).
  *** END INVARIANT ***

- Construct MountPayload value:
      MountPayload {
          payload_pin_count: AtomicU32::new(0),
          fs_ops: output.fs_ops,
          fs_page_backing: output.fs_page_backing,
          backing: block_device,
          dev_id,
          options: spec.options,
          fstype: spec.fstype,
          ...
      }
- Construct MountIdentity value:
      MountIdentity {
          parent: AtomicSlot::new(MountParent::Attached(parent_binding)),
          mountpoint: target_dentry_cap,
          root_dentry: root_dentry_cap,
          mnt_ns: mnt_ns_binding,
          children: DllContainer::new(),
          child_chain: DllNode::unlinked(),
          flags: AtomicU32::new(spec.flags.bits()),
          propagation: AtomicU8::new(0),
          payload: PayloadBinding::pending(),
          umount_port: RawPort::new(),
      }
```

The `parent_binding` and `mnt_ns_binding` are the substrate-level
binding handles — Cap-bumping wrappers over `parent_mount_cap` and
`mnt_ns_cap`.

**Phase 4 — commit.** Class-3 compositional. Five linearization
points, ordered:

```text
1. payload_cap := zone::sign(payload_slot, payload_value)
   -> Cap<MountPayload>
   wrap: payload_pcap := PayloadCap::from(payload_cap)
2. identity_cap := zone::sign(identity_slot,
       identity_value.with_payload(payload_pcap))
   -> Cap<MountIdentity>
3. *** LINEARIZATION POINT ***
   index::install_if_absent(
       mnt_ns.mountpoint_index, target_key, identity_cap.clone())
   New walker probes observe the crossing from this commit point
   onward.
4. index::install_if_match(
       parent_mount.children, identity_cap.clone())
   Derived materialization. Justified by identity.parent's value
   (set in step 2).
5. index::install_if_match(
       mnt_ns.all_mounts, identity_cap.clone())
   Derived materialization. Justified by identity.mnt_ns.
```

**Phase 5 — publish.** No fire — there is no SIG attachment for
"new mount" in v1 (would require an `mnt_event_port` for fsnotify
consumers; deferred). The materialization being live in
`mountpoint_index` is what the next walker probe sees.

**Outcome.** `StepOutcome::Done(identity_cap)`.

### 5.2 `step_mount_bootstrap`

<!-- txdoc:MOUNT-STEP-MOUNT-BOOTSTRAP-1 -->

Install the root mount of a fresh MountNamespace. Used at boot
exclusively. See §7 for the surrounding boot sequence.

```rust
pub fn step_mount_bootstrap(
    spec: BootstrapMountSpec,
) -> StepOutcome<(Cap<MountNamespace>, Cap<MountIdentity>)>;

pub struct BootstrapMountSpec {
    pub fstype:  &'static str,
    pub flags:   MountFlags,
    pub options: MountOptions,
    pub source:  Option<Arc<dyn BlockDevice>>,    // pre-resolved
}
```

**Differences from `step_mount`:**

- No target-resolution phase. There is no parent namespace in
  which to resolve `/`.
- No CAP_SYS_ADMIN check. Bootstrap runs from kernel init context;
  there is no userspace caller.
- A fresh `MountNamespace` is allocated and signed as part of the
  same step.
- The new MountIdentity's `parent` is set to `MountParent::Root`.
- The new MountIdentity's `mountpoint` is a self-reference
  sentinel: at construction, `mountpoint = root_dentry_cap.clone()`.
  This makes `..` from the namespace root's root_dentry stay put
  (consistency with the chroot/mnt_ns_root rule in VFS_CHECKS §6.1
  rule 2; and consistent with `MountParent::Root` in
  `synthesize_dotdot_cross`, §4.3.4).
- `mountpoint_index` install is **skipped**. Root mounts have no
  index entry; they are reached only via
  `MountNamespace.root_mount`.
- `parent.children` install is skipped (no parent).
- `MountNamespace.all_mounts` install **does** happen — the root
  mount appears in `/proc/mounts` enumeration.

**Bootstrap cyclic construction.** A subtle but real issue: the
namespace and the root mount reference each other —
`MountNamespace.root_mount` holds `Cap<MountIdentity>`, and
`MountIdentity.mnt_ns` holds `Binding<MountNamespace,
Addressability>`. Neither can be signed referencing the other if the
other doesn't yet have a Cap. `step_mount_bootstrap` resolves this
with a documented **cyclic-sign protocol** that is allowed only
during boot:

```text
Cyclic-sign protocol (BOOTSTRAP-ONLY):

  1. Reserve all three slots: ns_slot, identity_slot, payload_slot.
  2. Construct the unsigned values by their reservation tokens.
     The mutual references are written as substrate "cyclic
     bindings" — substrate-level forward references that are
     materialized into Caps at sign time, not at value
     construction.

         identity_value.mnt_ns_token = ns_slot.token()
         ns_value.root_mount_token   = identity_slot.token()

     The token APIs return zone-internal handles that resolve to
     Cap<T> only after the corresponding sign primitive runs.

  3. Run FS-driver mount_init (same as step_mount phase 3).
     Build root_rnode, attach to root_dentry. Both unpublished.

  4. Sign in payload-first order:
       payload_cap  := zone::sign(payload_slot, payload_value)
       identity_cap := zone::sign(identity_slot,
           identity_value.with_payload(payload_cap)
                         .with_mnt_ns_resolved(/* deferred */))
       Wait — ordering issue. See below.

  5. The actual ordering uses zone::sign_cyclic_pair:

       (ns_cap, identity_cap) := zone::sign_cyclic_pair(
           ns_slot,       ns_value,
           identity_slot, identity_value,
           bind_resolution = |id_cap, ns_cap| {
               // Atomically: identity.mnt_ns := Binding(ns_cap)
               //             ns.root_mount   := id_cap
               // Both bindings installed before either Cap escapes
               // the substrate primitive.
           })

     This substrate primitive performs both signs as one atomic
     transaction with respect to external observers; only after
     both signs commit does either Cap become observable outside
     the primitive.

  6. all_mounts DLL install: index::install_if_match(
         ns_cap.all_mounts, identity_cap.clone()).

  7. Globally publish ns_cap as the system root namespace.
```

`zone::sign_cyclic_pair` is a substrate primitive **reserved for
this case**. It is not part of the general substrate primitive
vocabulary. The closed-catalog rule in CONCEPTS §15.4 admits it as
a special case for cyclic-binding bootstrap; ordinary mounts use
ordinary `zone::sign` and have no need for cyclic resolution.

Three reasons the cyclic case is bootstrap-only:

- Only at bootstrap is the entire identity-payload-namespace triple
  freshly allocated; runtime mounts attach into an existing
  namespace whose `Cap<MountNamespace>` is already valid.
- Only at bootstrap does a MountNamespace not yet have a
  root_mount; runtime mounts always have a parent mount in the
  existing namespace and bind through that.
- The publication discipline (ARCH-5) is preserved because
  `sign_cyclic_pair` is itself the linearization point — observers
  see either the pre-bootstrap state (no root namespace) or the
  post-bootstrap state (both Caps live). No partial visibility.

**Alternative** (for reviewers preferring not to add a substrate
primitive): the same effect can be achieved by signing the
identity with `mnt_ns: AtomicSlot<Option<Binding<MountNamespace,
Addressability>>>` and filling in via expected-old swap after
the namespace is signed. This costs an Option layer on every
runtime read of `mnt_ns`. The cyclic-pair primitive avoids the
runtime cost but adds substrate surface area. v1 picks the
cyclic-pair approach because mnt_ns reads are walker-hot
(`lookup_mount_at` re-validates `mount.mnt_ns` on every probe).

**Outcome.** `StepOutcome::Done((mnt_ns_cap, identity_cap))`.

### 5.3 `step_umount_lazy` (MNT_DETACH)

<!-- txdoc:MOUNT-STEP-UMOUNT-LAZY-MNT-DETACH-1 -->

Detach a mount from its namespace. Existing operational users
continue; payload reclaims when pin count drops.

```rust
pub fn step_umount_lazy(
    target: Path,
    ctx:    &ThreadContext,
) -> StepOutcome<()>;
```

**Phase 1 — observe.**

```text
- cred::checks::require_cap_sys_admin(ctx)
- mount::checks::require_mount_point(target, ctx)
    Witness: target must be a mount root in current namespace.
    Returns EINVAL if not a mount point.
- mount::checks::require_no_child_mounts(witness.mount)
    Predicate: mount.children DLL is empty. Returns EBUSY if not.
    (Recursive umount is a script over multiple step_umount_lazy
    calls in leaf-first order.)
- mount::checks::require_not_namespace_root(
      witness.mount, ctx.frame.mount_ns)
    Predicate: mount != mnt_ns.root_mount. Returns EINVAL if root.
    (The root mount has no parent to fall back to; pivot_root is
    deferred to Phase 2.)
```

**Phase 2 — upgrade.**

```rust
let mount_cap: Cap<MountIdentity> = upgrade(witness.mount)?;
let mnt_ns_cap: Cap<MountNamespace> = upgrade(witness.mnt_ns)?;
```

**Phase 3 — reserve.** No reservations. Umount only withdraws.

**Phase 4 — commit.**

```text
1. *** VISIBILITY BOUNDARY 1 ***
   index::withdraw_commit(
       mnt_ns.mountpoint_index, dentry_key,
       expected: mount_cap.clone())
   Authoritative crossing binding withdrawn. New walkers do not
   cross. (MOUNT-COVER lifted, mountpoint DEntry uncovered.)
2. index::withdraw_commit(
       parent_mount.children, expected: mount_cap.clone())
3. index::withdraw_commit(
       mnt_ns.all_mounts, expected: mount_cap.clone())
4. mount.parent.swap(MountParent::Detached)
   AtomicSlot mutation: Attached(parent_binding) -> Detached.
   The parent_binding's Drop releases the parent Cap. Future
   ".." traversals across this parent edge return ENOENT
   (DotDotResult::DetachedFail per §4.3.4).
5. (DO NOT touch mount.payload.)
   PayloadBinding remains Some. MountPayload stays alive.
   payload_pin_count holders continue using the payload.
```

**Phase 5 — publish.**

```rust
mount.umount_port.fire(UmountEvent::Detached);
```

Per SIGNAL_ATTACHMENTS §3.8. Subscribers wake and re-observe under
fresh guard (see §3.6 for the race walkthrough).

**Subsequent payload reclamation.** The PayloadBinding is dropped
when `Cap<MountIdentity>` refcount drops to zero — which happens
after every consumer (walker IdentRefs, Frame.cwd/root if they
reached into this mount, OpenFiles' RNode chains) releases their
identity Cap. At that point:

- PayloadBinding's Drop fires.
- MountPayload's payload-projection contributors are then just
  `payload_pin_count`.
- When `payload_pin_count` reaches 0 and the PayloadBinding is
  gone, MountPayload's compound payload predicate falls to false.
- SENTINEL_DEAD CAS commits on MountPayload.
- `fs_ops::shutdown` runs in the reclaim queue: page cache
  flushed, journal closed, block device released.
- MountPayload zone slot reclaims after epoch quiescence.

**Outcome.** `StepOutcome::Done(())`.

### 5.4 `step_umount_normal` (busy-check)

<!-- txdoc:MOUNT-STEP-UMOUNT-NORMAL-BUSY-CHECK-1 -->

Like `step_umount_lazy`, but rejects if the mount has active
operational users. EBUSY on conflict.

```rust
pub fn step_umount_normal(
    target: Path,
    ctx:    &ThreadContext,
) -> StepOutcome<()>;
```

**Differences from `step_umount_lazy`:**

- Phase 1 adds `mount::checks::no_active_payload_users(payload)`:
  predicate `MountPayload.payload_pin_count.load(Acquire) == 0`.
  Returns EBUSY if not.
- Phases 2–5 identical to `step_umount_lazy`.

**v1 approximation.** A perfectly-correct check would scan all
`Cap<MountIdentity>` and `Cap<MountPayload>` holders globally,
which is impractical. The v1 approximation:

- Walker-held IdentRefs are guard-scoped, so they don't show up in
  the count between syscalls. (Concurrent walkers mid-syscall do;
  acceptable race.)
- `Frame.cwd: Cap<RNode>` and `Frame.fs_context.root: Cap<RNode>`
  references RNodes whose containing MountPayload contributes to
  `payload_pin_count`.
- `OpenFile`'s RNode chain contributes when constructed.
- `PageContainerKind::File`'s `Cap<MountPayload>` contributes
  directly.

So the "no active users" condition reduces to "no syscall-visible
operational evidence." Concurrent walkers may briefly cause
spurious EBUSY; the caller can retry or use MNT_DETACH. This
matches Linux's pragmatic approximation.

**Failure mode.** `StepOutcome::Err(EBUSY)`.

### 5.5 `step_unshare_mnt_ns` (v1: ABI-only)

<!-- txdoc:MOUNT-STEP-UNSHARE-MNT-NS-V1-ABI-ONLY-1 -->

```rust
pub fn step_unshare_mnt_ns(
    ctx: &ThreadContext,
) -> StepOutcome<Cap<MountNamespace>>;
```

**v1 implementation.**

- Phase 1: `cred::checks::require_cap_sys_admin(ctx)`. Returns
  EPERM if denied.
- Phase 2–4: returns the existing `Frame.mount_ns` Cap unchanged.
  No clone.
- Phase 5: no fire.

**Outcome.** `StepOutcome::Done(ctx.frame.mount_ns.clone())`.

Phase 2 will replace this with the actual COW clone path:

- Allocate a new MountNamespace slot.
- Clone `mountpoint_index` BTree by root-pointer copy.
- Duplicate every MountIdentity in the source namespace, rewriting
  `mnt_ns` bindings to point at the new namespace.
- Each duplicated MountIdentity shares the source's MountPayload
  (PayloadCap clone — bind-mount-like).

### 5.6 `step_setns_mnt`

<!-- txdoc:MOUNT-STEP-SETNS-MNT-1 -->

```rust
pub fn step_setns_mnt(
    target_ns: Cap<MountNamespace>,
    ctx:       &ThreadContext,
) -> StepOutcome<()>;
```

**v1 implementation.**

- Phase 1: `cred::checks::require_cap_sys_admin(ctx)`.
- Phase 1: validate `target_ns` is the global root namespace
  (the only one that exists in v1).
- Phase 4: write `Frame.mount_ns := target_ns`. Effectively a
  no-op-with-validation since target_ns must equal the existing
  value.

**Outcome.** `StepOutcome::Done(())` or `Err(EINVAL)`.

### 5.7 `clone_mnt_ns` (helper, called by fork)

<!-- txdoc:MOUNT-CLONE-MNT-NS-HELPER-CALLED-FORK-1 -->

Not a standalone step — invoked from `proc::execution::fork_reserve`
when handling Frame inheritance.

```rust
pub fn clone_mnt_ns(
    parent_ns: &Cap<MountNamespace>,
    flags:     CloneFlags,
) -> Cap<MountNamespace>;
```

**v1 implementation.** Bumps refcount on `parent_ns` and returns a
fresh `Cap<MountNamespace>`. The CLONE_NEWNS bit is ignored
(handling deferred to Phase 2).

```rust
pub fn clone_mnt_ns(
    parent_ns: &Cap<MountNamespace>,
    _flags:    CloneFlags,
) -> Cap<MountNamespace> {
    parent_ns.clone()
}
```

The function exists so PROCESS_v1's `fork_reserve` has a stable
hook; Phase 2 lights up CLONE_NEWNS handling without the fork code
needing a structural change.

### 5.8 Deferred steps

<!-- txdoc:MOUNT-DEFERRED-STEPS-1 -->

- **`step_pivot_root`** — Topology shuffle (root_mount, two parent
  edges, two index entries) needs slot-locked-with-re-read for
  atomicity across the four bindings. Deferred for separate spec
  pass. v1 returns ENOSYS.
- **`step_umount_force` (MNT_FORCE)** — Active payload
  invalidation; in-flight semantics subtle. Deferred.
- **`step_remount` (MS_REMOUNT)** — Requires published-slot
  discipline for `MountPayload.options`. Deferred.
- **`step_move_mount` (MS_MOVE)** — Re-parent a mount; needs
  Phase 2 parent-binding mutation. Deferred.
- **`step_bind_mount` (MS_BIND)** — Same MountPayload, distinct
  MountIdentity. Trivial extension on v1's entity factoring.
  Deferred per MOUNT-11.

---

## 6. Cross-cutting patterns

<!-- txdoc:MOUNT-CROSS-CUTTING-PATTERNS-1 -->

Worked examples illustrating mount's interactions with the rest of
the kernel. Each example walks the relevant code path step by step
and verifies the architectural invariants hold.

### 6.1 Lazy umount with active reader

<!-- txdoc:MOUNT-LAZY-UMOUNT-ACTIVE-READER-1 -->

**Scenario.** Process A has `/mnt/data/file` open for reading; an
mmap maps the file's first page into A's address space. Process B
runs `umount -l /mnt/data` (lazy umount).

**Setup.**
- `/mnt/data` is mountpoint dentry MD; mounted there is M
  (MountIdentity Cap_M, MountPayload Cap_MP).
- A holds `OpenFile { rnode: Cap<RN_file>, ... }`. RN_file's
  backing is `PageContainerKind::File { fs: Cap_MP, fs_object_id: 17 }`.
  This Cap_MP contributes to `MP.payload_pin_count` (count = 1).
- A has `VmEntry` referencing a page from `RN_file`'s PageContainer;
  the PC also holds Cap_MP (count = 2).
- A's PTE for this page is installed and resident.

**Execution.**

```text
T=0: B issues umount(-l /mnt/data).
     step_umount_lazy(/mnt/data, B's ctx).

T=1: Phase 1 checks pass (B has CAP_SYS_ADMIN, M is a mount root,
     M has no children, M is not the namespace root).

T=2: Phase 2 upgrade: Cap<MountIdentity> on M succeeds.

T=3: Phase 4 commit:
     1. mountpoint_index withdraws (MD -> M) entry.
        ** Visibility boundary 1 reached. **
        New walker probes for /mnt/data return None.
     2. parent.children withdraws.
     3. mnt_ns.all_mounts withdraws.
        /proc/mounts no longer lists M.
     4. M.parent.swap(MountParent::Detached).
     5. M.payload remains Some.

T=4: Phase 5: M.umount_port.fire(Detached).

T=5: Step returns Done(()).

T=6: A continues running. A reads from the mmap'd page.
     - VmEntry resolves to the same PC.
     - PC's Cap_MP keeps MP alive.
     - read() through OpenFile.rnode hits the page cache, served
       from existing pages.
     - For pages not in cache, fs_page_backing.fetch_page is
       called (still alive, fs_ops still functional).
     A's reads succeed normally.

T=7: A unmaps the mmap and closes the file.
     - VmEntry teardown drops PC ref; if last, PC reclaims.
       PC's Cap_MP drops; MP.payload_pin_count -> 1.
     - OpenFile drops; OpenFile's RN_file Cap drops; if last, RN_file
       reclaims. RN_file's backing's Cap_MP drops;
       MP.payload_pin_count -> 0.

T=8: MP.payload_pin_count is now 0.
     M's identity Cap is also dropped (no walker, no Frame.cwd,
     nothing references M).
     Identity SENTINEL_DEAD attempted; succeeds.
     M.payload (PayloadBinding Drop) releases the PayloadCap on MP.
     MP.payload predicate now false (no PayloadCap, no pins).
     SENTINEL_DEAD CAS on MP succeeds.
     fs_ops.shutdown() runs: cache flushed, journal closed, block
     device released.

T=9: After epoch quiescence: MP slot reclaims; M slot reclaims.
```

**Invariants verified:**

- **MOUNT-7** (no retargeting). A's open file and mmap continued
  serving correctly across the umount; reads landed against the
  same RN_file as before.
- **MOUNT-8** (namespace withdrawn before payload dropped).
  Visibility boundary 1 (T=3) precedes payload drop (T=8) by an
  arbitrary interval, governed by A's lifecycle.
- **`MountIdentity.namespace ⟂ MountPayload.payload`.** Between T=3
  and T=8, namespace was false, payload was true. The orthogonality
  is observable.

### 6.2 Mount over a directory with an open file in it

<!-- txdoc:MOUNT-MOUNT-OVER-DIRECTORY-OPEN-FILE-IT-1 -->

**Scenario.** Process A has `/mnt/old/file` open; process B runs
`mount -t tmpfs none /mnt`. The mount covers `/mnt`'s contents.

**Setup.**
- `/mnt` is dentry MD with children including `old/`.
- A holds `OpenFile` against RNode for `/mnt/old/file`. The Cap
  chain runs through `/mnt/old/file` RNode -> `/mnt/old` DEntry ->
  `/mnt` DEntry. Each operational binding holds appropriate
  evidence.

**Execution.**

```text
T=0: B issues mount(none, /mnt, tmpfs, 0, NULL).
     step_mount(spec).

T=1: Phase 1: target_dentry := MD. Not currently mounted.

T=2: Phase 4 commit installs:
     mountpoint_index[MD] -> M_tmpfs.
     ** From now on, walkers traversing /mnt see the tmpfs root. **

T=3: Walker for path "/mnt/new/file" (post-mount):
     - resolves "mnt" to MD.
     - lookup_mount_at(MD) returns Some(M_tmpfs).
     - walker pushes MountBoundary, cursor = M_tmpfs.root_dentry.
     - resolves "new" against tmpfs's root -> ENOENT (empty fs).

T=4: A continues holding /mnt/old/file open. A's OpenFile.rnode
     is unchanged; reading and writing continue to work against
     the underlying RNode (which lives in the original filesystem,
     reachable by Cap chain even though no path leads to it now).

T=5: A's read/write/close all succeed normally.

T=6: B issues umount(-l /mnt).
     step_umount_lazy.
     mountpoint_index[MD] -> M_tmpfs withdraws.

T=7: Walker for "/mnt/old/file" (post-umount):
     - resolves "mnt" to MD.
     - lookup_mount_at(MD) returns None.
     - walker proceeds into MD's children, finds "old", continues.
     - Resolves to the same RNode A is holding.
     The original filesystem is uncovered.
```

**Invariants verified:**

- **MOUNT-3** (covered DEntry's contents hidden). Between T=2 and
  T=6, lookups under /mnt landed in tmpfs's empty root, not in the
  underlying directory.
- **MOUNT-7** (no retargeting). A's open file remained valid
  throughout.
- **Uncovering on umount.** After T=6, paths under /mnt resolve
  back to the original directory contents.

### 6.3 Rename of an active mountpoint

<!-- txdoc:MOUNT-RENAME-ACTIVE-MOUNTPOINT-1 -->

**Scenario.** Mountpoint `/mnt/m` has filesystem M attached.
Userspace tries `rename("/mnt/m", "/mnt/m2")`.

**Execution.**

```text
T=0: rename syscall dispatches to vfs::execution::step_rename.

T=1: Phase 1: vfs::checks::require_parent_and_named_child for old
     side. base = (parent=/mnt, child=/mnt/m).

T=2: vfs::checks's refinement: is_mountpoint_in(child=/mnt/m,
     mnt_ns) -> true. (M is in mountpoint_index.)
     Returns EBUSY.

T=3: Step returns Err(EBUSY).
```

The rename never reaches commit. (MOUNT-6 satisfied.)

### 6.4 `..` from inside a mount with empty trail

<!-- txdoc:MOUNT-FROM-INSIDE-MOUNT-EMPTY-TRAIL-1 -->

**Scenario.** Process A has `cwd == /mnt/data` (a mounted
filesystem's root). A calls `openat(AT_FDCWD, "..", O_RDONLY)`.

**Execution.**

```text
T=0: walk_to_completion(mode=Entity, path="..", root_ctx={
        cwd: M_data.root_dentry,    // A's cwd is the mount root
        current_mount: M_data,      // initialized from cwd's mount
        ...
     }).

T=1: WalkState init:
        cursor = M_data.root_dentry
        current_mount = M_data
        trail = []
        remaining = [".."]

T=2: kernel_step consumes "..":
     trail is empty.
     Cursor is not at chroot or mnt_ns_root (it's at M_data.root,
     which is below the namespace root).
     is_mount_root(cursor, mnt_ns) -> true.
     synthesize_dotdot_cross(M_data, mnt_ns, g):
        M_data.parent.load() = MountParent::Attached(parent_binding).
        Reads parent_mount = parent_binding.cap = M_parent.
        Reads mountpoint = M_data.mountpoint = MD (the dentry
        /mnt/data is mounted on, in M_parent's filesystem).
        Returns DotDotResult::Cross(DotDotCross {
                    parent_mount: M_parent, mountpoint: MD }).
     Walker:
        cursor := MD.parent (via dcache parent cache; this is
                  M_parent's /mnt dentry).
        current_mount := M_parent.

T=3: remaining is empty; accepts. cursor is /mnt in M_parent.
     Witness: EntityAtPath { dentry: /mnt, rnode: ..., mount: M_parent }.

T=4: openat returns fd opened on /mnt.
```

(MOUNT-4 satisfied including the empty-trail edge case.)

### 6.5 Walker mid-crossing during MNT_DETACH

<!-- txdoc:MOUNT-WALKER-MID-CROSSING-DURING-MNT-DETACH-1 -->

**Scenario.** Walker A is mid-traversal across mount M (cursor is
inside M, trail has MountBoundary). B fires MNT_DETACH on M.
NeedIO yield happens just before B's commit.

**Execution.**

```text
T=0: A's WalkState:
        cursor = some_dentry inside M
        current_mount = M
        trail = [MountBoundary { was_at: MD, was_in_mount: M_parent },
                DEntry(some_path_dentry), ...]

T=1: A's kernel_step needs to lookup a child not in cache. NeedIO.
     Walker upgrades:
        cursor -> Cap<DEntry>
        current_mount -> Cap<MountIdentity> (Cap_M)
        trail entries -> Caps
     Packages ResumeToken. Releases guard. Yields.

T=2: B's step_umount_lazy commits.
     mountpoint_index withdraws. M.parent set to None.
     M.payload still Some.

T=3: I/O completes. A's reactor wakes A.
     A acquires fresh guard. Downgrades Caps in ResumeToken to
     IdentRefs under new guard.
     A's WalkState reconstructed. cursor and current_mount valid
     (their Caps survived the yield, anchoring the slots).
     kernel_step re-predicates cursor:
        namespace_live(cursor, root_ctx)?
     The cursor's DEntry is still alive (A's Cap kept it alive).
     But its namespace projection: is it still reachable from
     mnt_ns_root via the binding chain?
     The chain is: mnt_ns.root_mount -> ... -> M_parent -> MD ->
     M (via mountpoint_index) -> M.root_dentry -> ... -> cursor.
     The mountpoint_index[MD] -> M binding is now withdrawn.
     namespace_live returns false.
     kernel_step returns Error(DetachedNamespace).
     classify -> ENOENT.

T=4: Walker returns ENOENT to the syscall script.
```

The walker fails cleanly with ENOENT after the umount. Race-
degradation theorem holds. (Compare with the simpler interleaving
in §3.6 where the walker doesn't yield.)

### 6.6 Detached mount with cwd still inside it

<!-- txdoc:MOUNT-DETACHED-MOUNT-CWD-STILL-INSIDE-IT-1 -->

**Scenario.** Mount M was MNT_DETACH'd. Process A had
`Frame.cwd: Cap<RNode>` (paired with `Frame.cwd_mount:
Cap<MountIdentity>` per §12) pointing inside M when the umount
happened.

The semantics here distinguish **global namespace reachability**
from **origin-held addressability**. Lazy umount lowers the
former — fresh absolute walks from `/` no longer reach M. It does
**not** lower the latter — A's cwd is an origin-held cursor and
relative walks proceed from it.

```text
Global namespace reachability (used for absolute paths and
/proc-style global projections):
    A path "/x/y/z" reaches an entity only if every binding on
    the chain mnt_ns.root_mount -> ... -> entity is currently
    valid. After detach, the chain through M is broken; absolute
    paths cannot enter M.

Origin-held addressability (used for relative walks from cwd,
chroot, or directory-fd origins):
    A walk starting at an existing Cap<DEntry> does not require
    a chain back to mnt_ns.root_mount. It requires only that the
    starting cursor's identity is alive (held by the origin's
    Cap) and that each step's local binding (parent->child within
    M) is alive.

After lazy umount of M:
    fresh absolute walks: no longer reach M (global reachability
        broken).
    relative walks from a held cursor inside M: continue working
        within M while M.payload remains Some and the local
        bindings stay valid.
    `..` upward across M's detached parent edge: governed by the
        v1 detached-parent policy (§4.3.4: DotDotResult::Detached
        -> ENOENT in v1).
    getcwd: may fail because global namespace reconstruction
        from cwd back to mnt_ns_root cannot succeed.
```

**Execution walkthrough.**

```text
T=0: M is detached. M.payload still Some (held by A's cwd_mount/
     cwd_mount_payload_pin pair, by RNodes in M, by any
     PageContainerKind::File pointing at M's payload).

T=1: A calls open("./file"):
     - RootCtx.cwd      = IdentRef from A.frame.cwd Cap.
     - RootCtx.cwd_mount uses A.frame.cwd_mount Cap.
     - Walker initializes current_mount = cwd_mount.
     - kernel_step does NOT perform a global-reachability check
       on cursor (it would fail). It re-predicates using the
       origin-held variant: cursor's identity is alive (Cap held
       by the cwd path), local namespace_live within M is
       checked at each forward step (M's internal DEntry tree
       is intact).
     - "file" lookup proceeds via M.fs_ops or cache.
     - Open succeeds.

T=2: A calls open("/etc/passwd"):
     - RootCtx.chroot or mnt_ns_root used as origin.
     - Walker proceeds along the global path; never enters M.
     - Open succeeds against /etc/passwd in the still-attached
       filesystems.

T=3: A calls open("../sibling"):
     - Walker starts at cwd inside M.
     - Consumes "..".
     - Walker's trail is empty; cursor is the cwd's DEntry.
     - If cursor is at M.root_dentry: synthesize_dotdot_cross
       returns DotDotResult::Detached (because
       M.parent == MountParent::Detached).
     - v1 policy: walker emits Error(DetachedNamespace),
       classify -> ENOENT.
     - If cursor is below M.root_dentry: normal `..` within M
       succeeds (this is internal to the detached mount).

T=4: A calls getcwd():
     - getcwd renders cwd as an absolute path, which requires
       walking from cwd up to mnt_ns_root following parent edges.
     - At M.root_dentry the parent edge is Detached.
     - getcwd fails with ENOENT (matches Linux behavior for cwd
       in unreachable directory).

T=5: A calls chdir("/somewhere_else"):
     - Fresh absolute walk from mnt_ns_root.
     - A.frame.cwd and A.frame.cwd_mount updated.
     - Old Cap<RNode> and old Cap<MountIdentity> drop.
     - cwd_mount_payload_pin drops; M.payload's pin count --.
     - When all pins released and identity Cap drops, M and MP
       reclaim per the standard chain.
```

**Invariants verified:**

- **MOUNT-7** (no retargeting). A's cwd-anchored open continued
  serving correctly within the detached mount. The path argument
  resolved to the same entities it would have resolved to before
  the detach.
- **Lazy umount means fresh-only.** New paths from outside cannot
  enter M; held cursors continue serving relative paths inside M.
  The orthogonality of `MountIdentity.namespace ⟂
  MountPayload.payload` admits this case directly: namespace is
  false, payload is true, relative walks consult payload.

**Required walker-side support.** This walkthrough requires the
walker's namespace-reachability check to distinguish two modes:

```text
mode = GlobalReachable:
    Used for absolute paths from "/", from chroot, from
    mnt_ns_root, or from any walk whose origin is the namespace
    root.
    Predicate namespace_live(d, ctx) requires the binding chain
    from mnt_ns.root_mount (or chroot) to d to be valid.

mode = OriginHeld:
    Used for relative paths from a held cursor: cwd, an open
    directory fd, an *at fd anchor.
    Predicate namespace_live_local(d, ctx) requires only:
        - d's identity Cap is alive (witnessed by the origin's
          Cap chain).
        - For each forward step P -> child, the parent->child
          binding within the same mount must be valid (catches
          unlink/rename/rmdir within the detached mount).
    No requirement that mnt_ns.root_mount reaches d.
```

The walker's `kernel_step` selects the mode based on the origin
of the walk (mnt_ns_root/chroot vs cwd/fd-anchor). This is a
**cross-doc edit on VFS_CHECKS** §6.1 rule 1 (re-predicate cursor
on entry); recorded in §12.

### 6.7 Stat across mount boundary

<!-- txdoc:MOUNT-STAT-ACROSS-MOUNT-BOUNDARY-1 -->

**Scenario.** `stat("/mnt/data/file", &buf)`. `/mnt/data` is a
separate mount.

**Execution.**

```text
T=0: vfs walker resolves to EntityAtPath {
        dentry: file_dentry,
        rnode: file_rnode,
        mount: M_data,    // the mount containing file_rnode
     }.

T=1: stat syscall script:
     buf.st_dev = M_data.payload.observe(g).dev_id;
     buf.st_ino = file_rnode.fs_object_id;
     ... (other fields from rnode/inode_meta) ...

T=2: For "/etc/file" (in root mount):
     EntityAtPath.mount = root_mount.
     buf.st_dev = root_mount_payload.dev_id.
     Different value.
```

(MOUNT-5 satisfied.)

---

## 7. Boot sequence

<!-- txdoc:MOUNT-BOOT-SEQUENCE-1 -->

The mount subsystem is brought up after the substrate, allocator,
device registry, and reactor are initialized. Boot constructs the
root MountNamespace and mounts the early filesystem layer.

### 7.1 Boot order (mount-subsystem view)

<!-- txdoc:MOUNT-BOOT-ORDER-MOUNT-SUBSYSTEM-VIEW-1 -->

```text
Phase B0: substrate, HAL, allocator up.        (foundation)
Phase B1: zone allocators registered.          (foundation)
Phase B2: reactor initialized.                 (foundation)
Phase B3: block-device drivers register.       (DEVICE.md §6)
Phase B4: bdev-fs constructs MountPayload.     (BDEV_FS.md §7.1)
          Note: bdev-fs's MountPayload is built BEFORE any
          MountNamespace exists. It is held by the global bdev-fs
          static and only becomes a "real mount" in phase B5b.
Phase B5: mount subsystem init:
          B5a. mount::execution::init() runs.
               Allocates the root MountNamespace's zone slot.
               Creates anon_dev_bitmap.
               No mount yet — the namespace is empty.
          B5b. step_mount_bootstrap(rootfs_spec).
               rootfs_spec is determined by kernel command line
               or build config:
                 - tmpfs (early initramfs scenario)
                 - bdev-fs (boot from raw block device)
                 - ext4 (when block device + ext4 are both ready)
               Returns (mnt_ns_cap, root_mount_cap).
               Globally publishes mnt_ns_cap as the system root
               namespace.
          B5c. step_mount for each early pseudo-fs:
                 - bdev-fs at /dev/block       (BDEV_FS.md §7.1)
                 - devfs at /dev               (DEVICE.md §6)
                 - procfs at /proc
                 - devpts at /dev/pts          (TTY.md)
                 - sysfs at /sys (if enabled)
               Each mount uses the same step_mount as runtime
               mount; no special privilege beyond running in init
               context.
Phase B6: open /dev/console for pid1's stdin/stdout/stderr.
Phase B7: construct pid1 Frame:
          Frame {
              ...
              mount_ns: mnt_ns_cap.clone(),
              cwd: root_mount.root_dentry's RNode,
              cwd_mount: root_mount.clone(),
              fs_context: FsContext {
                  root: root_mount.root_dentry's RNode,
                  root_mount: root_mount.clone(),
                  ...
              },
              ...
          }
Phase B8: hand off to pid1.
```

Steps B5a–B5c are the mount-subsystem responsibilities. B6–B8
are PROCESS_v1's responsibilities; they consume the Cap returned
by B5b.

### 7.2 Bootstrap-specific concerns

<!-- txdoc:MOUNT-BOOTSTRAP-SPECIFIC-CONCERNS-1 -->

- **Root mount has `parent: AtomicSlot(MountParent::Root)`.** The
  three-state encoding makes the root distinguishable from a
  detached mount even though both have "no parent." `..` at the
  root_dentry returns `DotDotResult::StayAtRoot` (cursor stays).
  Phase 2 pivot_root mutates this slot.
- **Root mount's `mnt_ns` is filled by the cyclic-sign protocol.**
  Per §5.2, `step_mount_bootstrap` uses `zone::sign_cyclic_pair`
  which materializes both `MountIdentity.mnt_ns` (Binding) and
  `MountNamespace.root_mount` (Cap) atomically before either Cap
  escapes the substrate primitive. This is the only context in
  which a runtime `Cap<MountNamespace>` mounts itself.
- **Root mount's `mountpoint`** is a self-reference: at construction,
  `mountpoint = root_dentry.clone()`. This makes `..` from the
  namespace root's root_dentry stay put per VFS_CHECKS §6.1 rule 2
  ("If trail empty and cursor at chroot or mnt_ns_root, cursor
  stays" — mnt_ns_root is `mnt_ns.root_mount.root_dentry`, which
  is exactly cursor in this case).
- **No `mountpoint_index` entry.** Root mounts have no entry. They
  are reached only via `MountNamespace.root_mount`. Subsequent
  mounts (e.g. devfs at /dev) are normal: their `mountpoint` is a
  DEntry in some parent mount (the root mount), and they get a
  normal `mountpoint_index` entry.
- **Root mount is in `all_mounts`.** It appears in `/proc/mounts`
  with target "/" and source determined by fstype.
- **Root mount cannot be unmounted.** `step_umount_lazy` and
  `step_umount_normal` reject (M == mnt_ns.root_mount) with EINVAL
  via `require_not_namespace_root`. To "replace" the root mount,
  use pivot_root (Phase 2).

### 7.3 Mount table at boot completion

<!-- txdoc:MOUNT-MOUNT-TABLE-BOOT-COMPLETION-1 -->

After phase B5c completes, a typical v1 boot has:

```text
mountpoint_index entries (in the global MountNamespace):
    /dev        -> devfs MountIdentity
    /dev/block  -> bdev-fs MountIdentity
    /dev/pts    -> devpts MountIdentity
    /proc       -> procfs MountIdentity
    /sys        -> sysfs MountIdentity (if enabled)
    [user mounts added later]

MountNamespace.root_mount: rootfs MountIdentity (no index entry)

all_mounts DLL: [rootfs, devfs, bdev-fs, devpts, procfs, sysfs, ...]
```

`/proc/mounts` enumerates this list in mount order.

### 7.4 What boot does NOT do

<!-- txdoc:MOUNT-WHAT-BOOT-DOES-NOT-DO-1 -->

- Boot does **not** poke `mountpoint_index` directly.
  `step_mount_bootstrap` and `step_mount` are the only paths.
- Boot does **not** construct MountIdentity slots ad-hoc. The
  steps allocate via zone primitives.
- Boot does **not** take shortcuts on the publication discipline.
  The phase-4 commit ordering is the same in `step_mount_bootstrap`
  as in `step_mount`, modulo the differences listed in §5.2.

This uniformity makes the boot path testable by the same machinery
as runtime mount: mock the FS drivers, run the steps, assert
post-conditions.

---

## 8. POSIX alignment

<!-- txdoc:MOUNT-POSIX-ALIGNMENT-1 -->

Not every POSIX (or Linux 2.6) mount feature lands in v1. Per
MOUNT-11.

### 8.1 Committed in v1

<!-- txdoc:MOUNT-COMMITTED-IN-V1-1 -->

| Syscall / feature | v1 behavior |
|---|---|
| `mount(source, target, fstype, flags, data)` | `step_mount`. Supports all v1-listed fstypes (ext4, tmpfs, procfs, devfs, devpts, sysfs, bdev-fs internals). |
| `umount(target)` | `step_umount_normal`. Returns EBUSY if active payload users; otherwise detaches and reclaims. |
| `umount2(target, MNT_DETACH)` | `step_umount_lazy`. Always succeeds (modulo EINVAL/EPERM/EBUSY-no-children). |
| `mount(... MS_RDONLY ...)` | Sets MountFlags::RDONLY in `MountIdentity.flags`. Enforced via two-source check (§9). |
| `mount(... MS_NOSUID ...)` | Sets MountFlags::NOSUID. Enforced in execve cred recompute. |
| `mount(... MS_NODEV ...)` | Sets MountFlags::NODEV. Enforced in open of device-backed RNodes. |
| `mount(... MS_NOEXEC ...)` | Sets MountFlags::NOEXEC. Enforced in execve permission check. |
| `mount(... MS_NOATIME ...)` | Sets MountFlags::NOATIME. Enforced in atime-update path (suppress writes). |
| `unshare(CLONE_NEWNS)` | `step_unshare_mnt_ns`. Returns success with existing namespace Cap. |
| `setns(fd, CLONE_NEWNS)` | `step_setns_mnt`. Returns success if target is the existing namespace. |
| `stat`'s `st_dev` field | Read from `EntityAtPath.mount.payload.dev_id`. |
| `/proc/mounts` | `mount::project::render_proc_mounts(mnt_ns)`. |
| `/proc/<pid>/mountinfo` | `mount::project::render_proc_mountinfo(mnt_ns)`. |
| Recursive umount (script) | Composition over `step_umount_lazy` calls in leaf-first order, in `scripts/`. |

### 8.2 Returns ENOSYS in v1

<!-- txdoc:MOUNT-RETURNS-ENOSYS-V1-1 -->

| Syscall / feature | Reason |
|---|---|
| `pivot_root(new_root, put_old)` | Topology shuffle deferred. |
| `umount2(target, MNT_FORCE)` | Active payload invalidation deferred. |
| `mount(... MS_REMOUNT ...)` | Mid-flight option mutation deferred. |
| `mount(... MS_BIND ...)` | Bind mounts deferred per MOUNT-11. |
| `mount(... MS_MOVE ...)` | Re-parent deferred. |
| `mount(... MS_SHARED|SLAVE|PRIVATE ...)` | Propagation deferred. |
| `unshare(CLONE_NEWNS)` actual COW | Returns success but no new namespace; Phase 2. |
| `setns(fd, CLONE_NEWNS)` to a different ns | Returns EINVAL; only the existing ns is valid. |
| `open_tree`/`fsopen`/`fsmount`/`move_mount`/`mount_setattr` | Linux 5.2 new mount API; deferred. |
| `fchroot`-style operations | Out of v1 scope. |

### 8.3 Userspace expectations

<!-- txdoc:MOUNT-USERSPACE-EXPECTATIONS-1 -->

- **`mount(8)`** runs as root; its mount(2) calls succeed with v1
  for the supported flag/fstype combinations.
- **`umount(8)`** without `-l` calls `umount(2)` (normal). With
  `-l`, calls `umount2(target, MNT_DETACH)`.
- **`/etc/mtab`** is conventionally a symlink to `/proc/mounts`
  on Linux; v1 expects this convention (no kernel-side mtab
  maintenance).
- **`getmntent(3)`** parses `/proc/mounts`; works with v1's
  projection format.
- **`statvfs(2)`** consumes `EntityAtPath.mount.payload.fs_ops`
  via the FS backend's statvfs hook (deferred to TX_EXT4_PLAN's
  Phase 5; not strictly mount's responsibility).

---

## 9. Flag enforcement detail (MOUNT-12)

<!-- txdoc:MOUNT-FLAG-ENFORCEMENT-DETAIL-MOUNT-12-1 -->

```rust
bitflags! {
    pub struct MountFlags: u32 {
        const RDONLY  = 1 << 0;     // MS_RDONLY
        const NOSUID  = 1 << 1;     // MS_NOSUID
        const NODEV   = 1 << 2;     // MS_NODEV
        const NOEXEC  = 1 << 3;     // MS_NOEXEC
        const NOATIME = 1 << 4;     // MS_NOATIME
    }
}
```

### 9.1 Two-source read-only check

<!-- txdoc:MOUNT-TWO-SOURCE-READ-ONLY-CHECK-1 -->

A write operation against an RNode in mount M is denied if
**either**:

- **(a)** `M.flags` contains RDONLY (per-mount VFS policy), or
- **(b)** `M.payload.fs_ops.is_read_only()` (filesystem-level
  state — set by mount-time read-only flag on the FS, by failed
  journal recovery, by underlying block device read-only, or by
  remount-ro in Phase 2).

Either condition fires EROFS.

```rust
// mount::checks::predicates.rs
pub fn require_writable_mount<'g>(
    mount: IdentRef<'g, MountIdentity>,
    g:     &'g Guard,
) -> Result<(), Errno> {
    let flags = MountFlags::from_bits_truncate(mount.flags.load(Acquire));
    if flags.contains(MountFlags::RDONLY) {
        return Err(EROFS);
    }
    let payload = mount.payload.observe(g).ok_or(ESTALE)?;
    if payload.fs_ops.is_read_only() {
        return Err(EROFS);
    }
    Ok(())
}
```

The two sources are independent:

| Scenario | (a) | (b) |
|---|---|---|
| `mount -o ro` | true | false |
| Block device is read-only, FS rw-mounted | false | true |
| Failed journal recovery → FS forced ro | false | true |
| `mount -o ro` on a ro device | true | true |

Phase 2's `MS_REMOUNT` will publish a new flags value via
expected-old discipline on the AtomicU32; the predicate continues
to read the current value under Acquire.

### 9.2 Enforcement points

<!-- txdoc:MOUNT-ENFORCEMENT-POINTS-1 -->

| Flag | Predicate site | Error | Notes |
|---|---|---|---|
| RDONLY | `vfs::checks::require_writable_mount` (in open with O_WRONLY/O_RDWR/O_TRUNC, creat, link, unlink, rename, mkdir, rmdir, symlink, chmod, chown, truncate, utimes) | EROFS | Two-source check per §9.1 |
| NOSUID | `proc::execution::compute_new_cred` ignores S_ISUID/S_ISGID | (silent) | suid/sgid bits effectively zeroed |
| NODEV | `vfs::checks::require_device_open_permitted` (in open, if `RNode.backing` is `Device { ... }`) | EACCES | NOSUID+NODEV typical for /tmp |
| NOEXEC | `proc::checks::require_executable_witness` (in execve) | EACCES | applies to interpreters too |
| NOATIME | `vfs::execution::touch_atime_commit` short-circuits | (silent) | suppresses atime updates |

The flag values are read from `EntityAtPath.mount.flags` at the
syscall script's phase-1 observe. Flags are atomic, single-load.

### 9.3 Why flag enforcement lives in consumers, not mount

<!-- txdoc:MOUNT-WHY-FLAG-ENFORCEMENT-LIVES-CONSUMERS-NOT-MOUNT-1 -->

Mount publishes flags via `MountIdentity.flags` (a witness-
reachable atomic field). Consumers (VFS, Exec) read and enforce.
Mount does *not* implement the enforcement — that would couple
mount to specifics of write-paths, exec semantics, atime policy,
etc., which belong in their respective subsystems.

This keeps MOUNT-BDY-3 clean: mount supplies topology and flags;
consumers supply semantics.

---

## 10. MOUNT-1..12 satisfaction table

<!-- txdoc:MOUNT-MOUNT-1-12-SATISFACTION-TABLE-1 -->

| Rule | Statement | Satisfied by |
|---|---|---|
| MOUNT-1 | Mount attaches MountIdentity to a directory DEntry in a MountNamespace. | §5.1 (`step_mount`) phase 4 step 3: `mountpoint_index[dentry] -> identity` install in the namespace's index. |
| MOUNT-2 | While attached, path resolution crossing that DEntry enters the mounted filesystem root. | §4.2 covering invariant; §4.3.3 forward crossing in walker rule 3; §3.3 `lookup_mount_at` with consistency re-validation. |
| MOUNT-3 | Covered DEntry remains structurally present but its directory contents are hidden from path resolution. | §4.2 covering invariant: walker substitutes cursor before child lookup; covered DEntry's children container persists, ready to serve post-umount. |
| MOUNT-4 | `..` from a mounted root crosses back to the parent mount and mountpoint parent. | §4.3.4 `..` rule with empty-trail mount-root case via `synthesize_dotdot_cross`. |
| MOUNT-5 | `stat.st_dev` changes across mount boundaries; each MountPayload supplies a stable dev id. | §2.2 `MountPayload.dev_id` field; §6.7 stat walkthrough; `EntityAtPath.mount` field added in §4.3.5. |
| MOUNT-6 | Active mountpoints reject unlink/rmdir/ordinary rename with EBUSY. | §4.3.6 refinement wrappers `require_rmdirable_dir_child` and rename's `require_parent_and_named_child` add `is_mountpoint_in` -> EBUSY clauses. |
| MOUNT-7 | Existing open files, cwd/root cursors, and in-flight resolvers are not retargeted by mount or umount. | §6.1, §6.2 walkthroughs; §3.6 race-degradation theorem; observer contract per LIVENESS §2.6. |
| MOUNT-8 | Umount withdraws namespace reachability before dropping payload; detached-but-held mounts are valid degraded states. | §3.5 umount commit ordering: visibility boundary 1 (index withdraw) precedes payload drop; §6.1 walkthrough showing payload alive after detach. |
| MOUNT-9 | Mount/umount are privileged operations. | §5.1 `step_mount` phase 1 calls `cred::checks::require_cap_sys_admin`; same for `step_umount_lazy`, `step_umount_normal`, `step_unshare_mnt_ns`, `step_setns_mnt`. |
| MOUNT-10 | Mount table is authoritative; /proc/mounts is a projection, not separate state. | §11.1 boundary table (procfs row); §11.3 MOUNT-BDY-1 (mount as sole owner); `mount/project.rs` reads `all_mounts` under guard, no caching. |
| MOUNT-11 | Phase 1 supports only normal mount + normal/lazy umount; bind/move/propagation/namespaces are deferred. | §5.8 deferred steps; §8.2 ENOSYS table. |
| MOUNT-12 | Mount flags minimally include ro, nosuid, nodev, noexec. | §9 flag-enforcement detail; bitflags definition in §9; enforcement points table in §9.2. |

All twelve satisfied. Reviewer who finds a gap should treat it as
a spec bug and add a row identifying the missing connection.

---

## 11. Subsystem interaction boundaries

<!-- txdoc:MOUNT-SUBSYSTEM-INTERACTION-BOUNDARIES-1 -->

Mount sits between many subsystems. The table below catalogs what
mount provides to each peer, what it consumes, and what neither
side may do across the boundary. Three boundary invariants
(MOUNT-BDY-1..3) hold globally.

### 11.1 Boundary table

<!-- txdoc:MOUNT-BOUNDARY-TABLE-1 -->

| Peer subsystem | Mount provides | Mount consumes | Must not cross |
|---|---|---|---|
| **VFS** | `lookup_mount_at`, `is_mount_root`, `is_mountpoint_in`, `synthesize_dotdot_cross`, `require_mount_point`; `MountPayload.fs_ops`; `dev_id` | DEntry/RNode witnesses, mountpoint path resolution | VFS must not inspect `mount::structure`; Mount must not own DEntry/RNode lifecycle |
| **FS backend** | mounted instance host, `MountInitContext`, `MountPayload` lifetime | `FsOps`, `FsPageBacking`, root fs object | Backend must not construct RNodes or resolve global paths |
| **Process** | `Cap<MountNamespace>` for Frame; namespace clone/setns steps | current process cred/capability for mount permission | Process must not inspect mount tree |
| **Exec** | mount flags through VFS witness: NOEXEC/NOSUID | executable RNode/open evidence | Exec must not resolve mountpoints itself |
| **VM / PageBacked** | `Cap<MountPayload>` as file backing identity | page fetch/flush through `FsPageBacking` | Mount must not touch pmap/PTEs |
| **Device / bdev-fs** | mounted FS instance over block handle | block-device RNode / `BlockDeviceHandle` | Mount must not call hardware driver directly |
| **procfs** | mount-table iterator / projection source | none except read viewpoint | procfs must not duplicate mount table state |
| **TTY / devfs / devpts** | ordinary mounted filesystems | StructBacked TTY RNodes via devfs/devpts | Mount must not implement terminal / session semantics |
| **Signal / Bus** | `umount_port` detach hint | `RawPort` publish primitive | Bus signal must not become authoritative truth |
| **Boot** | root namespace and mount install API | boot-provided root/dev/proc/tmp payloads | Boot must not mutate MountNamespace internals directly |

### 11.2 Notes on individual rows

<!-- txdoc:MOUNT-NOTES-INDIVIDUAL-ROWS-1 -->

**FS backend "Backend must not construct RNodes."** The FS backend
produces `FsObjectId` values and `InodeMeta`; VFS's
RNode-find-or-create protocol (TX_EXT4_PLAN §3.4) takes those and
constructs RNodes. The backend never holds `Cap<RNode>` and never
sees DEntries. The backend's universe is `(FsObjectId, name, byte
offset, block number)`; everything outside is mount/VFS.

**Process "must not inspect mount tree."** Process stores
`Cap<MountNamespace>` opaquely in Frame, passes it to the walker
via RootCtx, and uses it as a refcount target for fork/unshare. It
never reads `mountpoint_index`, `all_mounts`, or any MountIdentity
fields directly. The single exception is `clone_mnt_ns` calling
into `mount::execution`, which is the sanctioned clone API.

**VM/PageBacked "Mount must not touch pmap/PTEs."** The
Cap<MountPayload> embedded in `PageContainerKind::File` flows from
PageBacked outward; mount never inspects the PCs it's referenced
from, never enumerates them, never invalidates PTEs. Cache flush
on lazy umount is *driven by the pin-count discipline* — when
consumers (PCs, OpenFiles) drop, the count decrements; mount waits
for zero, never forces it in v1.

**procfs "must not duplicate mount table state."** This is
SUBSYSTEM_ANATOMY §2.4's projection rule applied to mount.
`/proc/mounts` reads `MountNamespace.all_mounts` on each open;
no caching, no shadow table. Even Linux's userspace `/etc/mtab`
is symlinked to `/proc/mounts` on modern systems for exactly this
reason.

**TTY/devfs/devpts "Mount must not implement terminal/session
semantics."** Mount provides the FS-instance hosting; devfs/devpts
implement `FsOps` which constructs StructBacked RNodes whose
payload is a Tty entity. The Tty subsystem owns Tty entity
lifetime, controlling-tty assignment, foreground pgrp, line
discipline, hangup, TIOCSCTTY. Mount has zero knowledge of any of
this.

**Signal/Bus "Bus signal must not become authoritative truth."**
`umount_port.fire(Detached)` is a hint. A subscriber wakes,
acquires a fresh epoch guard, re-observes
`MountNamespace.mountpoint_index` and `MountIdentity.payload`. Per
SIG-1, SIG-2: signals are not state, wires are not state.

**Boot "Boot must not mutate MountNamespace internals directly."**
Boot calls `step_mount_bootstrap` and `step_mount` like any other
client. It does not poke `mountpoint_index` directly to install
mounts, even though it's the only context where doing so wouldn't
race with anyone. The discipline is uniform: all mount installation
goes through steps, even at boot. This makes the boot path testable
by the same machinery as runtime mount.

### 11.3 The three boundary invariants

<!-- txdoc:MOUNT-THE-THREE-BOUNDARY-INVARIANTS-1 -->

These slot into the MOUNT-* rule space alongside MOUNT-1..12.
They are subsystem-local invariants (mount-doc-scoped); the
INVARIANTS catalog could promote them to a global category if
reviewers want them globally citable.

> **MOUNT-BDY-1.** Mount is the sole owner of mount topology:
> `MountNamespace.mountpoint_index`, `MountIdentity.parent`, and
> mount tree membership (`children` DLL, `all_mounts` DLL).

This is the **single-source-of-truth** rule for mount structure.
No other subsystem holds a parallel representation of the mount
tree, the mountpoint→mount mapping, or membership. Consequences:

- VFS's walker carries `current_mount: IdentRef<'g, MountIdentity>`
  in WalkState — that is *observation*, not ownership. The
  IdentRef is anchored to the walker's epoch guard and disappears
  at guard release.
- procfs's `/proc/mounts` rendering iterates `all_mounts` under
  guard — observation, not ownership. No procfs-side cache.
- Boot's bootstrap sequence calls `step_mount_bootstrap` once,
  then `step_mount` for subsequent mounts. Boot does not maintain
  a parallel "intended mount table" that mount mirrors.
- Process's `Frame.mount_ns: Cap<MountNamespace>` is a refcount
  handle, not topology. Process doesn't know which mounts are in
  the namespace, only that the namespace exists.

**Architectural test:** if the structure module were swapped out
(different BTree, different DLL choice), no other subsystem's
code should need to change. v1's module layout achieves this.

> **MOUNT-BDY-2.** Other subsystems consume mount topology only
> through guard-scoped checks or stable Caps: VFS walker gets
> `MountTraversal`; Process stores `Cap<MountNamespace>`; procfs
> iterates via projection.

This is the **observation discipline** rule. Consumers fall into
exactly three modes:

| Consumer | Mode | What they get |
|---|---|---|
| VFS walker (hot path) | guard-scoped check | `MountTraversal<'g>` from `lookup_mount_at` |
| Exec / stat / write checks | guard-scoped check via witness | `EntityAtPath.mount: IdentRef<'g, MountIdentity>` (read flags atomically) |
| Process Frame | stable Cap | `Cap<MountNamespace>`, `Cap<MountIdentity>` (cwd_mount/root_mount) |
| Page cache file backing | stable Cap | `Cap<MountPayload>` (counts toward `payload_pin_count`) |
| FS backend | stable Cap | `Cap<MountPayload>` self-reference via `MountInitContext.mount_id` |
| procfs `/proc/mounts` | guard-scoped projection | iterator over `all_mounts` under guard |
| umount_port subscribers | publication wake + re-observe | wake hint, then re-issue a guard-scoped check |

There is no fourth mode. No subsystem holds a long-lived raw
pointer to MountIdentity/MountPayload, no subsystem holds an
IdentRef across an `.await`, no subsystem caches a mount lookup
result beyond the step that consumed it. The reference hierarchy
from CONCEPTS §3 is the only admissible vocabulary.

This invariant is what makes umount safe: lazy umount detaches
the namespace reachability binding, but every legitimate
consumer's reference type tells you exactly when their hold
becomes stale.

> **MOUNT-BDY-3.** Mount never owns file objects, process objects,
> device drivers, or VM mappings: it hosts filesystem instances
> and supplies topology/viewpoint, but concrete object semantics
> remain with VFS, Process, Device, VM, or the FS backend.

This is the **scope-limiting** rule. Mount is *plumbing*. It
connects:

- DEntries (owned by VFS) ↔ filesystem instances (owned by FS
  backend impls).
- Block devices (owned by Device) ↔ filesystem mounts that consume
  them.
- Page cache backing identity (owned by PageBacked) ↔ filesystem
  instances supplying pages.

It owns none of the things it connects. Mount does not implement:

- `read`/`write`/`mmap`/`open`/`close` on files (VFS / VM).
- Path walking, dcache, RNode resolution (VFS).
- Block I/O, device probing, hardware initialization (Device).
- Process lifecycle, fork, exec, exit, wait (Process).
- Page allocation, PC management, PTE installation (PageBacked /
  VM).
- On-disk format: superblocks, inodes, journals, extents (FS
  backend).
- Terminal semantics, session/pgrp, ldisc (TTY).

Mount implements:

- Mount tree topology: parent-binding, child-DLL, mountpoint_index.
- Mount lifecycle: install via FS handshake, detach via lazy
  umount.
- Mount-flag *publication* (the actual enforcement check sites
  live in VFS/Exec).
- `/proc/mounts` projection.
- `dev_id` allocation.
- The walker's mount-crossing oracles.

**Architectural test:** if MOUNT_v1 grows a section about file
I/O semantics, on-disk format, process state, or hardware, that
section is in the wrong document.

### 11.4 Boundary invariants vs MOUNT-1..12

<!-- txdoc:MOUNT-BOUNDARY-INVARIANTS-MOUNT-1-12-1 -->

The two rule sets are orthogonal:

- **MOUNT-1..12** specify *what mount does* — the behavioral
  contract: when a mount is attached, when path crossing occurs,
  when umount detaches reachability, what flags enforce, what's
  in v1.
- **MOUNT-BDY-1..3** specify *what mount owns* — the structural
  contract: what state lives in mount, what other subsystems may
  and may not do with it.

A behavior-violating change might respect boundaries (e.g.,
adding `MS_REMOUNT` to v1 — extends MOUNT-12 territory but doesn't
reach into other subsystems). A boundary-violating change typically
cuts across multiple behaviors (e.g., having Process directly
maintain a "mount index cache" for fast lookup — violates BDY-1
even if the cache happens to track MOUNT-2's behavior correctly).

§§1–9 (behavior) and §11 (boundaries) are separate sections. The
MOUNT-1..12 satisfaction table sits in §10; the MOUNT-BDY-1..3
satisfaction is built into §11 itself (the table and the
prose-rules establish the invariants and demonstrate their
satisfaction simultaneously).

---

## 12. Cross-doc edits

<!-- txdoc:MOUNT-CROSS-DOC-EDITS-1 -->

The mount-subsystem design requires the following edits to other
architecture documents. Each is small and localized; most are
additive.

| Doc | Section | Edit |
|---|---|---|
| **PROCESS_v1** | §3 Frame | Add `mount_ns: Cap<MountNamespace>`. Trim the "Phase 2" comment for mount_ns specifically. |
| **PROCESS_v1** | §3 Frame / FsContext | Add paired fields: `cwd_mount: Cap<MountIdentity>`, `cwd_mount_payload_pin: MountPayloadPin`, and similarly `root_mount` / `root_mount_payload_pin` on `fs_context`. Updated atomically by chdir/chroot. Used by walker to initialize `current_mount` for relative walks (§6.6) and to keep MountPayload alive across lazy umount per §2.5's closed accounting model. |
| **PROCESS_v1** | §6 fork | Note: `clone_mnt_ns` is called in `fork_reserve` for `Frame.mount_ns`; CLONE_NEWNS handled trivially in v1. The cwd_mount and root_mount Caps and their `MountPayloadPin`s are also cloned (Cap-bump + new pin acquire). |
| **PROCESS_v1** | execve | Note: phase 1 reads `EntityAtPath.mount.flags` for NOEXEC; cred recompute reads it for NOSUID. |
| **VFS** (OpenFile) | structure | `OpenFile` gains `mount: Cap<MountIdentity>` and `mount_payload_pin: MountPayloadPin`. Acquired at open's phase 4 from `EntityAtPath.mount`; dropped at last fd close. Per §2.5's closed accounting model. Applies to all OpenFile variants (regular file, dirfd, synthetic /proc fd, devpts master/slave, etc.). |
| **VFS_CHECKS** | §5.1 WalkState | Add `current_mount: IdentRef<'g, MountIdentity>` field. |
| **VFS_CHECKS** | §5.4 WalkTrail | `TrailEntry::MountBoundary` gains `was_in_mount: IdentRef<'g, MountIdentity>` field. |
| **VFS_CHECKS** | §5.5 RootCtxRef | Add `mnt_ns: IdentRef<'g, MountNamespace>` field, and `cwd_mount: IdentRef<'g, MountIdentity>` / `root_mount: IdentRef<'g, MountIdentity>` (downgraded from Frame's Caps at walk start). The walker initializes `current_mount` from cwd_mount or root_mount depending on whether the walk is relative or absolute (§6.6). |
| **VFS_CHECKS** | §6.1 rule 2 (`..`) | Add empty-trail mount-root case calling `mount::checks::synthesize_dotdot_cross`; handle three `DotDotResult` cases (Cross, StayAtRoot, DetachedFail → ENOENT). |
| **VFS_CHECKS** | §6.1 rule 3 (named component) | Call `mount::checks::lookup_mount_at` after resolving D, before using D as next directory. Update `current_mount` on substitution. |
| **VFS_CHECKS** | §10.1 `require_mount_point` | Implementation in `mount::checks::require_mount_point`, re-exported by VFS. |
| **VFS_CHECKS** | §10.2 refinement wrappers | `require_rmdirable_dir_child`, `require_parent_and_named_child` (rename old) gain `is_mountpoint_in(child, mnt_ns)` clauses → EBUSY. |
| **VFS_CHECKS** | §6.1 rule 1 (re-predicate) | Distinguish two predicate modes: `GlobalReachable` (used for absolute walks from mnt_ns_root or chroot; requires the binding chain from mnt_ns root to cursor to be valid) and `OriginHeld` (used for relative walks from cwd or directory-fd anchors; requires only that cursor's identity is alive and that each forward step's local parent→child binding is valid). The `kernel_step`'s entry re-predicate selects based on walk origin. Per §6.6's detached-cwd semantics. |
| **VFS_CHECKS** | §11 EntityAtPath | Add `mount: IdentRef<'g, MountIdentity>` field. Build_witness copies `state.current_mount`. |
| **VFS** (new helper) | execution | Add `vfs::execution::create_orphan_dentry(kind, rnode_cap, reservation)` for mount-time root_dentry construction. The returned DEntry has `parent: None` and is reachable only through the in-flight step's local frame until the calling step publishes the mount. |
| **VFS** (RNode) | structure | Confirm: synthetic and StructBacked RNodes (procfs, devfs, devpts, TTY, pipe, socket) reach a containing `MountPayload` through the mount that constructed them (mount-time RNode coherence index). OpenFile holds the Cap and pin per the closed accounting model in §2.5. |
| **PAGE_BACKED** | §3.2 PageContainerKind::File | Confirm: `fs: Cap<MountPayload>` Cap counts toward `MountPayload.payload_pin_count`. PC construction increments via `MountPayloadPin::acquire`; PC drop decrements. |
| **SIGNAL_ATTACHMENTS** | §3.8 | Confirm wording: `umount_port` is hint, not authoritative; subscribers re-observe under fresh guard. |
| **substrate** (zone) | (new) | `zone::sign_cyclic_pair` primitive: atomically signs two zone slots whose values cyclically reference each other (root MountIdentity ↔ MountNamespace). Lint-enforced as bootstrap-only. Per §5.2's cyclic-construction handling. |

These edits are flagged here for the eventual integration commit;
they don't change semantics elsewhere, only thread the plumbing
through.

---

## 13. Phase 2 deferral list (consolidated)

<!-- txdoc:MOUNT-PHASE-2-DEFERRAL-LIST-CONSOLIDATED-1 -->

| Item | Reason |
|---|---|
| `step_pivot_root` | Topology shuffle needs slot-locked-with-re-read for `root_mount`; defer for separate spec pass. `MountNamespace.root_mount` becomes `AtomicSlot<Cap<MountIdentity>>` then. |
| `step_umount_force` (MNT_FORCE) | Active payload invalidation; subtle in-flight semantics; defer. |
| `step_remount` (MS_REMOUNT) | Mid-flight option mutation; needs published-slot for options; defer. |
| `step_move_mount` (MS_MOVE) | Re-parent a mount; needs Phase 2 parent-binding mutation under expected-old discipline. |
| `step_bind_mount` (MS_BIND) | Same MountPayload, multiple MountIdentities; trivial extension. |
| Real `step_unshare_mnt_ns` | BTree COW clone + per-mount Identity duplication; Phase 2 mount-namespace work. |
| Real `step_setns_mnt` | Cross-namespace move; needs Phase 2 unshare. |
| `MS_SHARED` / `MS_SLAVE` / `MS_PRIVATE` propagation | Linux 2.6.15 shared subtree; `MountIdentity.propagation` field reserved. |
| `open_tree` / `move_mount` / `fsopen` / `fsmount` / `mount_setattr` | Linux 5.2 new mount API. |
| OverlayFS / autofs / unionfs | FS-specific. |
| fsnotify / inotify on mount events | Consumer of `umount_port` not specified. |
| RNode-to-MountPayload back-link | Currently inferred from PageContainerKind::File; if procfs/devfs synth RNodes need a uniform back-link, add `RNode.containing_mount: Weak<MountPayload>`. v1 manages without. |

---

## 14. One-paragraph summary

<!-- txdoc:MOUNT-ONE-PARAGRAPH-SUMMARY-1 -->

> Mount is the resolution-topology subsystem. Its three entities
> are `MountIdentity` (with `parent: AtomicSlot<MountParent>`
> three-state encoding Root/Attached/Detached, mountpoint and
> root_dentry consistency-field Caps, mnt_ns binding, flags,
> umount_port), `MountPayload` (FsOps/FsPageBacking, dev_id,
> options, fstype, source_label, payload_pin_count), and
> `MountNamespace` (root_mount, mountpoint_index BTree,
> all_mounts DLL). The mountpoint_index is the authoritative
> crossing binding; walker probes it at every named component
> via `lookup_mount_at` and substitutes cursor on a consistency-
> revalidated lookup. Identity/Payload split per object_model
> §8.1.1 enables lazy umount: `step_umount_lazy` withdraws the
> index entry as visibility boundary 1, transitions
> `parent: Attached → Detached`, but leaves payload populated; the
> closed catalog of `MountPayloadPin` acquirers (OpenFile, FsContext
> cwd/root, PageContainerKind::File) keeps the payload alive while
> any consumer holds operational evidence. Lazy umount distinguishes
> global namespace reachability (lost) from origin-held
> addressability (preserved): fresh absolute walks no longer reach
> the detached mount, but cwd-anchored relative walks continue
> within it; `..` across the detached parent edge fails with ENOENT
> via `DotDotResult::DetachedFail`. `step_mount`'s phase-3 helpers
> (`create_orphan_dentry`, `find_or_create_rnode`,
> `attach_rnode_to_dentry`) leave their products unpublished until
> phase-4 `mountpoint_index` install — the sole visibility boundary
> for the new mount. Boot's `step_mount_bootstrap` uses the
> bootstrap-only `zone::sign_cyclic_pair` substrate primitive to
> resolve the root MountIdentity ↔ MountNamespace cyclic
> reference. Mount publishes flags
> (RDONLY/NOSUID/NODEV/NOEXEC/NOATIME) and dev_id via the
> witness-reachable `EntityAtPath.mount` field; the read-only check
> ORs per-mount VFS policy with FS-level state. Phase 2 deferrals:
> pivot_root, MNT_FORCE, MS_REMOUNT, bind/move/propagation, real
> unshare-with-clone. Three boundary invariants hold globally:
> MOUNT-BDY-1 (mount is the sole owner of mount topology),
> MOUNT-BDY-2 (consumers use guard-scoped checks or stable Caps),
> MOUNT-BDY-3 (mount hosts filesystem instances but owns no files,
> processes, drivers, or mappings). All twelve MOUNT-1..12 rules
> satisfied; cross-doc edits to PROCESS_v1, VFS_CHECKS, PAGE_BACKED,
> SIGNAL_ATTACHMENTS, and substrate listed in §12.

---

## References

<!-- txdoc:MOUNT-REFERENCES-1 -->

- `../00_meta-framework/object_model_v2.md` — Identity/Payload split (§8.1.1),
  reference hierarchy, reclamation.
- `../00_meta-framework/CONCEPTS_v4.md` — authoritative bindings vs derived
  materializations and publication rule.
- `../00_meta-framework/INVARIANTS_v4.md` — ARCH-5 (publication rule), BIF-* (BIF-1
  declaration of split, BIF-3 obligation matching, BIF-5
  single-carrier signal attachment), STEP-4 (five-phase
  discipline), SIG-* (publication rules).
- `../00_meta-framework/archived/LIVENESS_v2.1.md` — archived catalog rows for MountIdentity/Payload,
  partial order namespace ⟂ payload, race-degradation theorem
  (§2.5), observer contract (§2.6).
- `../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md` — four-module layout, five-phase
  discipline, substrate primitives (§4.5).
- `VFS_CHECKS_V2.1.md` — walker mount-crossing integration points
  (§5.1, §5.4, §5.5, §6.1, §10.1, §10.2, §11).
- `../04_process-signals/PROCESS_v1.md` — Frame's `mount_ns` slot (§3); `clone_mnt_ns`
  invocation in fork (§6).
- `../04_process-signals/SIGNAL_ATTACHMENTS_v1.md` §3.8 — `umount_port` attachment.
- `../01_substrate/BUS_v1.md` — `RawPort` semantics for `umount_port`.
- `BDEV_FS.md` — block-device pseudo-filesystem; source of
  block-device RNodes for mount(2).
- `TX_EXT4_PLAN_v1_2.md` §3 — `FsOps`, `FsPageBacking`,
  `MountInitContext`, `MetadataPcFactory`, `MountOutput`.
- `PAGE_BACKED_v1.md` §3.2 — `PageContainerKind::File { fs:
  Cap<MountPayload>, ... }`.
- `DEVICE.md` §6 — devfs as a `MountPayload`.
- `VM_v1_2.md` — recipes-vs-pmap layering as a precedent for the
  index-vs-consistency-fields authority model.
