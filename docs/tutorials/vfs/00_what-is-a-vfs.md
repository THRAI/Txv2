# Part 0 — What a VFS is, and the idea that's different

## The job of a VFS

A program calls `open("/etc/passwd", O_RDONLY)` and gets back a small integer.
It calls `read(fd, buf, 4096)` and bytes appear in `buf`. It never learns
whether `/etc/passwd` lives on an ext4 partition, an in-memory tmpfs, a network
share, or a synthetic file produced on the fly. That indirection — *one syscall
surface, many filesystems behind it* — is the Virtual File System.

A VFS has to answer four questions, and traditional Unix gives each its own
object:

| Question | Traditional object |
|---|---|
| *What is this file?* (its metadata, its kind, where its data lives) | `struct inode` |
| *What is this file called, and where does it sit in the tree?* | `struct dentry` |
| *What is this particular open of the file?* (offset, flags) | `struct file` |
| *What filesystem is mounted here, and how do I talk to it?* | `struct super_block` + `vfsmount` |

And two vtables make the whole thing polymorphic:

- `inode_operations` — `lookup`, `create`, `mkdir`, `unlink`, `rename`, …
- `file_operations` — `read`, `write`, `llseek`, `readdir`, …

When the VFS needs to look up a name, it calls `inode->i_op->lookup(...)`. When
it reads, it calls `file->f_op->read(...)`. The filesystem driver supplies the
function pointers; the VFS supplies the orchestration. This is the textbook
design, and txKernel keeps all of it.

## The classic flow

A read of `/tmp/foo` in a traditional kernel goes roughly:

```
open("/tmp/foo", O_RDONLY)
  └─ path walk: start at root dentry
       ├─ lookup "tmp"  → dentry → inode (a directory)
       └─ lookup "foo"  → dentry → inode (a regular file)
  └─ allocate struct file { inode, offset=0, flags=O_RDONLY }
  └─ install in fd table → return fd

read(fd, buf, n)
  └─ file = fdtable[fd]
  └─ file->f_op->read(file, buf, n)   // dispatch to the filesystem
  └─ copy bytes to user, advance file->offset
```

Every box here has a txKernel counterpart, and the orchestration is nearly
identical. What differs is *how each box is built*.

## The idea that's different

Look at `struct inode`. In a traditional kernel it is one allocation holding
*everything* about the file:

```c
struct inode {
    umode_t           i_mode;       // identity-ish: kind + permissions
    uid_t             i_uid;        // identity-ish: ownership
    unsigned long     i_ino;        // identity: inode number
    const struct inode_operations *i_op;   // capability: the vtable
    struct address_space *i_mapping;       // payload: the page cache
    unsigned int      i_nlink;      // identity: link count
    atomic_t          i_count;      // lifetime: in-core refcount
    // ...
};
```

Three different *kinds* of thing share this one allocation and this one
lifetime:

1. **Identity** — `i_ino`, `i_nlink`: what makes this *this* file, its place in
   the namespace.
2. **Capability** — `i_op`, `i_fop`: what operations it supports.
3. **Payload** — `i_mapping` and the pages hanging off it: the operational
   resources, potentially megabytes of cached data.

The fusion is convenient until the lifetimes diverge. And they *do* diverge —
constantly:

- **`unlink` of an open file.** `i_nlink` drops to 0 (identity is now nameless),
  but `i_count > 0` because a `struct file` still points at it. Linux keeps the
  inode and its pages alive on a technicality: the in-core refcount. The file is
  a *zombie* — no name, but fully operational — until the last `close`. The
  whole multi-megabyte `i_mapping` rides along, attached to an inode no path can
  reach.

- **`umount -f` of a busy filesystem.** The superblock owns the backing device
  and the inode cache. A forced unmount wants to release the device *now*, but
  path walks may still hold inode references. Linux handles this with elaborate
  refcount and `s_active` bookkeeping, because superblock identity (still
  referenced) and superblock payload (the device, the caches) need to die at
  different times.

These are not edge cases; they are the load-bearing semantics of a Unix
filesystem. In a fused design they are managed by *reading the same refcount to
mean different things at different times*.

txKernel's answer: **stop fusing them.** Split each object into an identity half
and a payload half, allocate them separately, reclaim them independently, and
give them different reference types so the compiler tracks which one you hold.

| Traditional | txKernel |
|---|---|
| One `struct inode`, one refcount, three roles | `RNode` (identity) with a `backing` that *routes* to payload |
| One `struct super_block` | `MountIdentity` + `MountPayload`, separately allocated |
| `i_count` means "keep the whole thing" | `Cap<T>` pins identity; payload pinned by its own evidence |
| Zombie = inode with `i_nlink==0, i_count>0` | Zombie = identity alive, payload predicate still true — a *state*, not a trick |

> **Traditional VFS vs txKernel.** The traditional VFS asks "is anyone using
> this inode?" and answers with a single `i_count`. txKernel asks two
> independent questions — "is this still named in the namespace?" and "does its
> payload still have users?" — and stores the answer to each separately. The
> unlinked-but-open file is the case where the two answers differ, and that is
> precisely where the single-refcount design strains.

## What the rest of the series does

- **Chapter 1** introduces txKernel's three VFS entities — `RNode`, `DEntry`,
  `OpenFile` — and the `Cap<T>` handle, mapping each to its traditional
  counterpart. The split is present but not yet named.
- **Chapter 2** names it: the identity/capability/payload decomposition, the
  reference hierarchy (`Weak` → `IdentRef` → `Cap` → `PayloadCap`), and the four
  concrete motivations. This is the chapter the whole series builds toward.
- **Chapters 3–6** show the split in action: the backend boundary (`FsOps`),
  path resolution (the walker), mount (the headline payoff), and the
  `open`/`read`/`write` path.
- **Chapters 7–9** are functional deep-dives into the machinery those chapters
  compressed: the page cache and `mmap`, the special-file / fd zoo (pipes, ttys,
  devices, eventfd/epoll/…), and blocking + mount internals.
- **Chapter 10** traces one story — `open` then `read` then `unlink`-while-open
  then `close` — through every layer, so you watch identity and payload part
  ways and rejoin.

Keep the unlinked-open-file in mind as you read. It is the example we return to,
because it is the simplest place where "identity and payload are the same thing"
stops being true.

## Source anchors

- Inode/dentry/file entities: `crates/tx-subsystems/src/vfs/structure.rs`
- The split's design rationale: `docs/design/00_meta-framework/object_model_v2.md` §3
- Mount identity/payload: `crates/tx-subsystems/src/mount/mod.rs:292,413`
