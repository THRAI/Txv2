# The txKernel VFS

A tutorial series on how txKernel builds a Virtual File System — the layer that
lets one set of file syscalls (`open`, `read`, `write`, `stat`, `unlink`, …)
drive many different filesystems (tmpfs, procfs, devfs, ext4, FAT) — and on the
one structural idea that makes txKernel's VFS different from a textbook one.

## Who this is for

Developers who know the traditional Unix/Linux VFS — the four objects
(`inode`, `dentry`, `file`, `superblock`), the operations vtables
(`inode_operations`, `file_operations`), path lookup (`namei`/`link_path_walk`),
the dentry cache, and mount points. You do not need to know txKernel first; each
chapter teaches the traditional concept, then shows txKernel's version next to
it.

## The one-sentence thesis

**A file's *identity* and its *payload* are different things with different
lifetimes, so txKernel stores and reclaims them separately.** Identity is *what
makes this the same file* — its inode number, its place in the directory tree,
what open descriptors point at. Payload is *what you operate on* — the page
cache, the pipe buffer, the filesystem's backing store. A traditional VFS fuses
the two into one allocation with one lifetime. txKernel splits them.

This is not gratuitous. The two hardest correctness problems in any VFS are
*exactly* identity-outlives-payload problems:

- **Unlink an open file.** `rm` removes the last name, but a process still has
  it open and keeps reading. The directory entry is gone; the bytes must stay.
  *Identity unlinked, payload alive.*
- **Force-unmount a busy filesystem.** `umount -f` (or `MNT_DETACH`) tears down
  the backing store while path walks are still in flight. The superblock's
  resources must release; in-flight resolvers must not dereference freed memory.
  *Payload reclaimed, identity must persist until the last resolver lets go.*

In a fused design these are special cases bolted on with flags and reference
counts. In txKernel they fall out of the type system: identity and payload are
*separately allocated*, and the reference types name which one you hold.

## How to read the code in this series

Code blocks are **simplified pseudocode** — error arms elided, some generics
dropped, control flow straightened — but **type names, field names, and method
names are accurate** and match the real source. Every chapter ends with
`file:line` anchors so you can read the real thing.

## The series

The series has two arcs. **Chapters 0–6** build the conceptual core: the
entities, the split, the backend boundary, path resolution, mount, and the
read/write path. **Chapters 7–9** are functional deep-dives into the machinery
those chapters compressed: the page cache, the special-file/fd zoo, and
blocking + mount internals. **Chapter 10** ties it all together.

| # | File | Topic |
|---|------|-------|
| 0 | [00_what-is-a-vfs.md](00_what-is-a-vfs.md) | The traditional four objects, and the idea that's different |
| 1 | [01_entity-model.md](01_entity-model.md) | `RNode`, `DEntry`, `OpenFile`: the entities and their handles |
| 2 | [02_payload-identity-split.md](02_payload-identity-split.md) | **The split** — the reference hierarchy and why it exists |
| 3 | [03_fsops-and-backends.md](03_fsops-and-backends.md) | `FsOps`: the backend boundary, and a gallery of backings |
| 4 | [04_path-resolution.md](04_path-resolution.md) | The walker: path lookup as a step state machine |
| 5 | [05_mount-and-composite.md](05_mount-and-composite.md) | Mount: the split's headline payoff (force-umount) |
| 6 | [06_open-read-write.md](06_open-read-write.md) | `open`/`read`/`write` end to end through the fd table |
| 7 | [07_page-cache-and-mmap.md](07_page-cache-and-mmap.md) | The page cache: demand paging, writeback, and `mmap` sharing |
| 8 | [08_special-files-and-fd-zoo.md](08_special-files-and-fd-zoo.md) | Pipes, ttys, devices, and the synthetic fds (eventfd/epoll/…) |
| 9 | [09_blocking-and-mount-internals.md](09_blocking-and-mount-internals.md) | Blocking/readiness/`poll`; mount propagation, namespaces, boot |
| 10 | [10_capstone.md](10_capstone.md) | Capstone: `open` → `read` → `unlink`-while-open → `close` |

A single-file consolidated **[TECHNICAL_REPORT.md](TECHNICAL_REPORT.md)** covers
the same material in report form for readers who prefer one long document.

## The mapping table

Every chapter returns to this. The whole series is an expansion of it.

| Traditional VFS | txKernel | Anchor |
|---|---|---|
| `struct inode` | `RNode` (identity) + `RNodeBacking` (payload routing) | `vfs/structure.rs:576` |
| inode number | `FsObjectId` | `vfs/structure.rs:95` |
| `stat` fields | `InodeMeta` (immutable snapshot) | `vfs/structure.rs:227` |
| `struct dentry` + dcache | `DEntry` + weak `children` map | `vfs/structure.rs:866` |
| `struct file` (open description) | `OpenFile`, held as `Cap<OpenFile>` | `vfs/structure.rs:1154` |
| fd table | `ProcessPayload.fds: BTreeMap<u32, Cap<OpenFile>>` | `process/structure.rs:1131` |
| superblock + `vfsmount` | `MountIdentity` + `MountPayload` (fully split) | `mount/mod.rs:413,292` |
| `inode_operations` + `file_operations` | `FsOps` trait, dispatched as `Arc<dyn FsOps>` | `vfs/execution.rs:66` |
| `address_space_operations` / page cache | `FsPageBacking` + `PageContainer` | `page_backed/mod.rs:390` |
| `filemap_fault` (file `mmap`) | `materialize_page` via `VmEntryBacking::Page` | `vm/structure/types.rs:1072` |
| pipe / fifo | `PipePayload` + `StructPayload::Pipe` | `pipe/mod.rs:131` |
| tty / line discipline | `TtyIdentity` + `StructPayload::Tty` | `tty/execution/` |
| char device (`/dev/null`) | `CharDeviceBinding` + `StructPayload::CharDevice` | `device.rs:53` |
| eventfd/timerfd/signalfd/epoll | non-`Rnode` `OpenFileBacking` variants | `vfs/structure.rs:1016` |
| wait-queue + `wake_up` | `WaitSource` + `RNodeWaitPoints` | `vfs/notification.rs:30` |
| mount propagation / namespaces | `Propagation` + `MountNamespace` | `mount/mod.rs:121,524` |
| rootfs + `populate_rootfs` | `mount_rootfs_tmpfs` + `unpack_initramfs` | `init.rs:557`, `initramfs.rs:88` |
| `link_path_walk` / `namei` | walker step state machine (`kernel_step`) | `vfs/resolution/step.rs:51` |
| `do_sys_open` | `step_open` / `sys_openat` | `vfs/walker.rs:202` |
| `iget` / inode cache | `materialise_rnode` + `containing_mount` weak ref | `vfs/execution.rs` |
| `permission()` / `may_open` | `Credential` + DAC predicates | `vfs/predicates.rs` |

## What is genuinely the same as a normal VFS

So you do not over-attribute novelty: the *shape* of the VFS is conventional.
There is an inode-like object, a dentry-like cache, an open-file object, a
per-process fd table, a backend operations vtable, a recursive path walker,
POSIX permission checks, and mount points. If you have read the Linux VFS, you
will recognise every box in the diagram. The novelty is concentrated in **one
place**: each box is split along the identity ⟂ payload axis, and the reference
types (`Cap`, `PayloadCap`, `Weak`, `IdentRef`) make that split explicit and
enforced. Chapter 2 is where that idea lives; everything else is how it plays
out.
