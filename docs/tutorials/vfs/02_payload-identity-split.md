# Part 2 — The payload/identity split

This is the chapter the series is built around. Chapter 1 showed you the
entities; now we name the idea that shapes them, walk the reference types that
encode it, and — most importantly — work through *why it exists*. The VFS is the
best place in the kernel to learn this, because the canonical motivating cases
(`unlink` of an open file, `umount -f` of a busy filesystem) are VFS problems.

## The three-way decomposition

txKernel's object model (`object_model_v2.md` §3) says every entity decomposes
into three layers:

> **Identity.** What makes the entity *this one*. Its slot, its stable name,
> what bindings point at. Pinned by `Cap<T>`.
>
> **Capability.** The operations the entity supports. FileOps, InodeOps,
> protocol handlers. Injected at open time for per-instance capability.
>
> **Payload.** The operational resources. Page cache, socket buffers, protocol
> state. Pinned by `T::OperationalEvidence`.

Map that onto a file:

| Layer | For a regular file | Where it lives in txKernel |
|---|---|---|
| Identity | "inode 42 on this mount" | `RNode { fs_object_id, meta, … }` |
| Capability | "how to read/write this kind of thing" | the `FsOps` vtable + `RNodeBacking` tag |
| Payload | the file's actual bytes | a `PageContainer`, pointed at by `backing` |

A traditional `struct inode` fuses all three into one allocation with one
refcount. txKernel keeps them as distinct things with distinct retention.

## The reference hierarchy

The split is enforced by *which reference type you hold*. There are four, and
they form a ladder (`object_model_v2.md` §4):

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
  (= Cap<T> co-located, or PayloadCap<T> indirected, or typed pins)
```

Each rung, in the words of the spec, with its VFS use:

- **`Weak<T>`** — "a zone-slot reference with a generation tag. Contributes no
  retention. Upgrade compares the stored generation against the slot's current
  generation; mismatch (slot was reused) returns None." *VFS use:* `DEntry`'s
  cached `children`, `DEntry.mounted`, `RNode.containing_mount`. All hints that
  may have gone stale and must not keep their target alive.

- **`IdentRef<'g, T>`** — "an epoch-guarded pointer. Produced by traversal.
  Lifetime `'g` is tied to the guard; the borrow checker prevents escape …
  Safe to dereference because the guard defers reclamation." *VFS use:* every
  hop of a path walk. While the walker holds an epoch `Guard`, it dereferences
  dentries and rnodes through borrows that *cannot* escape the guard — so no
  refcount is touched per component.

- **`Cap<T>`** — "a refcounted pin on identity … `'static` with respect to
  epochs. Can be stored, sent, held across `.await`." *VFS use:* the references
  that *persist* — `OpenFile`'s `Cap<RNode>`, `DEntry`'s `Cap<RNode>` and
  `Cap<DEntry>` parent. These keep identity alive across time.

- **`T::OperationalEvidence`** — pins the *payload*. For entities whose identity
  and payload share a slot, this is just `Cap<T>`. For entities with a separate
  payload box it is `PayloadCap<T>`, which "entails `Cap<T>` by structural
  invariant: the payload box internally references its identity slot and cannot
  outlive it." For payload that is live under a disjunction of contributions
  (an inode is live if it still has links *or* still has opens), each
  contribution is a typed pin (`LinkPin`, `OpenPin`, …).

The ladder has a one-way property worth pausing on: **holding a higher rung
entails every lower rung.** Operational evidence entails an identity `Cap`
entails a safe dereference entails a weak ref. Downgrade is always free;
upgrade may fail (the thing died). That asymmetry is the whole game.

> **Traditional VFS vs txKernel.** Linux has *two* reference strengths on an
> inode: a real reference (`igrab`, bumps `i_count`) and "I looked at it under
> RCU." txKernel has *four* rungs, and critically it separates *identity*
> retention (`Cap`) from *payload* retention (`OperationalEvidence`). The Linux
> `i_count` conflates them: you cannot hold "keep the inode number valid" while
> letting "the page cache" go. txKernel can.

## Why split? Four motivations

The split costs something — two allocations where there was one, two reasons a
slot might not be freed yet, the discipline that guards must not span `.await`
(`object_model_v2.md` §2.6). It pays for itself four times over.

### Motivation 1 — Unlinked-but-open files (identity ⟂ namespace ⟂ payload)

POSIX requires that `unlink`ing a file you have open removes the *name* but
keeps the *bytes* until you `close`. The object model states the independence
directly: an inode's payload is live under a *disjunction*
(`object_model_v2.md` §3.3):

```
Inode.payload_live  ⇔  nlinks > 0  ∨  open_refs > 0
```

Links and opens are counted by *separate* typed pins (`LinkPin`, `OpenPin`).
`unlink` drops a `LinkPin`; `close` drops an `OpenPin`. The payload reclaims
only when *both* counts hit zero.

Now watch the references. In txKernel terms:

- The `DEntry` "foo" holds a `Cap<RNode>` — this is the *name* edge.
- The `OpenFile` holds a `Cap<RNode>` (inside `OpenFileBacking::Rnode`) — this
  is the *open* edge.

`unlink("/tmp/foo")` removes the `DEntry` from `/tmp`'s children and drops the
name edge's `Cap<RNode>`. But the `OpenFile`'s `Cap<RNode>` still pins the
`RNode` identity, and the page-cache `OpenPin` still pins the `PageContainer`
payload. The file is now reachable through exactly one of its two chains. Reads
keep working. At `close`, the last `Cap<RNode>` and the `OpenPin` drop together,
and *now* the backend's `destroy_inode` reclaims the storage.

In Linux this is the `i_nlink == 0 && i_count > 0` zombie, kept alive because
the single refcount happens to be non-zero. In txKernel it is not a coincidence
of one counter; it is two independently-counted predicates, and the type system
records which edge each holder owns. **The split makes "identity outlived its
name, payload outlived both" a representable state instead of a refcount
accident.**

### Motivation 2 — Force-unmount of a busy filesystem

This is the headline (Chapter 5 is the full treatment; here is why it needs the
split). `umount -f` / `MNT_DETACH` must release a filesystem's backing store —
the block device, the `FsOps` instance, the page-cache machinery — *even while
path walks are still in flight inside it*. But those in-flight walkers hold
references into the mount; freeing the backing under them would be
use-after-free.

txKernel splits the superblock into two separately-allocated objects:

- **`MountIdentity`** — the mount's place in the tree: its id, its mountpoint
  dentry, its root rnode, its parent. Held by resolvers as `Cap<MountIdentity>`.
- **`MountPayload`** — the operational resources: `fs_ops`, `fs_page_backing`,
  the block device. Pinned by `PayloadCap<MountPayload>` (via a
  `PayloadBinding`).

A forced unmount drops the payload binding: `MountPayload` reclaims *now*, the
device is released, no new operations can start. But any resolver still holding
`Cap<MountIdentity>` keeps the *identity* alive; its attempts to upgrade to the
payload simply fail cleanly (the binding is gone), and it unwinds with an error
rather than dereferencing freed memory. Identity persists until the last
resolver lets go.

This is the §5.4 cross-axis independence rule applied to mounts: identity
liveness and payload liveness are governed by *independent* mechanisms, and all
combinations — including "identity alive, payload gone" — are valid, handled
states.

### Motivation 3 — Refcount-free traversal on the hot path

Path resolution and fd lookup are the hottest paths in a filesystem. A
traditional dcache walk bumps and drops a refcount (or takes/releases RCU) at
*every* component. txKernel does neither on the way down. From the spec
(`object_model_v2.md` §2.5):

> Hot traversal paths (path resolution, fd lookup, pid lookup) are
> refcount-free. Each hop is an epoch-guarded pointer dereference; no atomic
> RMW.

The walker takes *one* epoch `Guard` for the whole walk. Each component is an
`IdentRef<'g, _>` borrow — a plain pointer dereference, memory-safe because the
guard defers reclamation of anything it might touch. Only at the *terminal* —
when the walk succeeds and the result must outlive the guard — does it upgrade
the final `IdentRef` to a `Cap` (one atomic, once per lookup, not once per
component).

This is only sound because identity and payload are split. The guard makes
dereferencing safe *regardless of payload state*; the walker never has to pin
payload just to read a name. EBR's classic problem — long-lived references
stalling epoch advance — is dodged because the guard is "type-bounded to stack
scope via Rust lifetimes. An epoch-derived reference cannot escape the guard
that produced it." (§2.1)

> **Traditional VFS vs txKernel.** Linux solved hot-path lookup with RCU-walk
> (`rcu_walk` / `lockref`), falling back to ref-walk when it hits something it
> can't do locklessly. txKernel's epoch guard is the same *idea* — defer
> reclamation so readers can run lock-free — but the `IdentRef`/`Cap` split
> makes "I'm just looking" vs "I'm keeping this" a type distinction the compiler
> checks, rather than a mode the walker switches between at runtime.

### Motivation 4 — Clean race degradation via monotone projections

When you split liveness into independent predicates, you need concurrent
operations to compose without resurrecting dead state. The model's answer is
that the predicates are *monotone*: each transitions true→false once and never
back (`object_model_v2.md` §5). A name, once unlinked, stays unlinked; a
payload, once reclaimed, stays reclaimed.

This is what makes the upgrade step (`IdentRef` → `Cap`, via a "SENTINEL_DEAD
CAS") meaningful. If a walker observed a dentry as live under its guard and then
tries to pin it, the CAS either succeeds (it was still live at the
linearization point) or fails because the slot went dead. A failure is a
*genuine* failure — never a race against a revival, because revival cannot
happen. The operation fails cleanly with `ENOENT`/`ESTALE` instead of pinning a
zombie. Splitting identity from payload is what lets each predicate be
independently monotone.

## The three factoring choices the VFS makes

The split is a *spectrum*, not a binary. The object model gives a rule
(`object_model_v2.md` §3.2, §8.1.1): split only where the lifetimes genuinely
diverge; co-locate where they don't. The VFS uses all three points on the
spectrum, and each choice is deliberate.

### Co-located: `DEntry` and `OpenFile`

> Entities with small, bounded payload and no zombie-semantics requirement
> (DEntry, FdEntry, OpenFile) co-locate identity and payload in a single slot.
> For these, `Cap<T>` and operational retention coincide.

A `DEntry` has no "alive but degraded" state — it is either a present name or it
is gone. An `OpenFile` is either an open description you can do I/O on, or it is
closed. Neither has a payload that outlives its identity. So both are single
allocations, and their `OperationalEvidence` *is* `Cap<T>`. Splitting them would
add cost for a lifetime divergence that never occurs.

### Special case: `RNode`

`RNode` is neither fully co-located nor fully split — it is *identity with a
payload pointer*. The `RNode` allocation holds only identity (`fs_object_id`,
`meta`) and a `backing` tag. The payload lives elsewhere and the tag routes to
it:

- `PageBacked { pc: Cap<PageContainer> }` — payload is a separate page
  container, with its own `OpenPin`/`LinkPin` lifetime.
- `StructBacked { payload }` — payload is a pipe/tty/socket struct, owned by its
  subsystem.
- `Projected { schema, key }` — there *is* no stored payload; bytes are
  synthesised on read (procfs/sysfs).
- `Symlink { target }` / `Directory` — tiny or no payload, inline.

So `RNode` gets the benefit of the split (identity is a small, uniform, stable
object; payload reclaims on its own schedule) without a second fixed "payload
struct" — because the *shape* of the payload varies by backing. Think of
`RNode` as always-identity, with `backing` as the doorway. This is the design
that makes Motivation 1 work: the page cache is reachable through `backing`, so
it can outlive every name without the `RNode` having to carry it inline.

### Fully split: `Mount`

`MountIdentity` and `MountPayload` are *separate zone allocations* (Chapter 5).
This is the maximal split, justified by Motivation 2: force-umount demands that
payload reclaim while identity persists. Here the divergence is not just
possible but *the entire point of the feature*, so the two halves get two
allocations and two reference types (`Cap<MountIdentity>`,
`PayloadCap<MountPayload>`).

| Entity | Factoring | Why |
|---|---|---|
| `DEntry` | co-located | no degraded state; name is present or gone |
| `OpenFile` | co-located | open or closed; nothing in between |
| `RNode` | identity + routed payload | payload shape varies; must outlive names |
| `Mount` | fully split (two allocations) | force-umount: payload dies, identity lingers |

## The payoff, stated once

Every hard VFS lifetime problem is "identity and payload want to die at
different times." A fused design fights this with one overloaded refcount and a
pile of special cases. txKernel's split makes the divergence *the normal case
the types are built for*: hold a `Cap` when you mean identity, hold operational
evidence when you mean payload, hold a `Weak` when you mean "a hint, maybe
stale," and borrow an `IdentRef` when you mean "just looking, under a guard."
The rest of the series is this principle worked out through `FsOps`, the walker,
mount, and the read/write path.

## Source anchors

- Identity / capability / payload: `docs/design/00_meta-framework/object_model_v2.md` §3 (`txdoc:OBJECT-MODEL-IDENTITY-CAPABILITY-PAYLOAD-1`)
- Co-located vs indirected: same doc §3.2 (`txdoc:OBJECT-MODEL-PAYLOAD-PLACEMENT-1`)
- Compound payload predicates (`LinkPin`/`OpenPin`): same doc §3.3 (`txdoc:OBJECT-MODEL-COMPOUND-PAYLOAD-PREDICATES-1`)
- Reference hierarchy: same doc §4
- EBR + pinning, refcount-free traversal: same doc §2
- Monotone projections: same doc §5
- Bifurcation rule (when to split): same doc §8.1.1
- VFS reference-type uses: `crates/tx-subsystems/src/vfs/structure.rs:480,576,866,1016`
- Mount split: `crates/tx-subsystems/src/mount/mod.rs:292,413`
