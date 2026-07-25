# Part 7 — Capstone: a file's whole life

One story, traced through every layer: a program opens a file on tmpfs, reads
it, `unlink`s it *while it is still open*, reads again, then closes. This is the
scenario Chapter 0 promised and the one the payload/identity split exists for.
Watch identity and payload part ways at the `unlink` and rejoin at the `close`.

The program:

```c
int fd = open("/tmp/foo", O_RDONLY);   // [A]
read(fd, buf, 100);                     // [B]
unlink("/tmp/foo");                     // [C]   ← removes the name, fd still open
read(fd, buf, 100);                     // [D]   ← still works! reads the same bytes
close(fd);                              // [E]   ← now the inode is finally freed
```

Assume `/tmp` is a tmpfs mount holding `foo`, a 100-byte regular file (inode
`FsObjectId(42)`, data in a `PageContainer`).

## [A] `open("/tmp/foo", O_RDONLY)`

`sys_openat` runs the Chapter 6 sequence.

1. Copy `"/tmp/foo"` from user; decode `O_RDONLY` → `OpenFileFlags { read:
   true, .. }`.
2. Absolute path → start at the process `cwd()`'s mount root.
3. **Walk** (Chapter 4). One epoch `Guard` for the whole walk:
   - component `tmp`: `kernel_step` checks search permission on `/`, looks it up
     — the `DEntry` for `/tmp` carries a `mounted_hint`. The walker upgrades it
     to the tmpfs `Cap<MountIdentity>` and **crosses the mount** (Chapter 5),
     switching `current` to the tmpfs root and `fs_ops` to tmpfs's.
   - component `foo`: tmpfs `lookup(root_id, "foo")` returns `Done(FsObjectId(42))`
     (an in-memory `BTreeMap` hit — no `Yield`). The walker loads `InodeMeta`,
     materialises a `Cap<RNode>` with `backing = PageBacked { pc }`, builds a
     `Cap<DEntry>` "foo", and caches it weakly in `/tmp`'s children.
   - end of path → `PathResolution { dentry, rnode, .. }`. Only here does the
     terminal `IdentRef` upgrade to `Cap` — one pin, not one per component.
4. `step_open` checks `O_RDONLY` against `foo`'s mode bits, then
   `OpenFile::new_cap_with_dentry(rnode, flags, dentry)`.
5. `fd = allocate_fd(); set_fd(fd, Some(openfile))`. Return `fd`.

The live graph now has **two chains** reaching inode 42:

```
   process.fds[fd] ─► Cap<OpenFile> ──► Cap<RNode>(42) ──► PageBacked{ pc } ──► Cap<PageContainer>
                          (open edge)        ▲                                    (the 100 bytes)
                                             │
   /tmp ─► DEntry "foo" ────────────────────┘
              (name edge)
```

Identity (`RNode(42)`) is pinned by *both* a `Cap` on the open edge and a `Cap`
on the name edge. Payload (the `PageContainer`) is pinned through `backing`.

## [B] `read(fd, buf, 100)`

`sys_read` (Chapter 6):

1. `resolve_fd(fd)` → the `Cap<OpenFile>`.
2. Backing is `Rnode`, and the `RNode` is `PageBacked`, so the page-backed read
   lane runs: `step_read` → `page_backed::step_read_to_kernel(pc, self, out,
   guard)`. Pages are already resident (tmpfs), so no `Yield`; 100 bytes land in
   the kernel buffer, the `offset` advances to 100.
3. `bootstrap_copy_to_user(buf, &bytes)`. Return 100.

Nothing surprising — but note the read reached the bytes through `OpenFile →
Cap<RNode> → backing → PageContainer`. It used the *open* chain, not the name.
That is about to matter.

## [C] `unlink("/tmp/foo")` — the split happens

`UnlinkOp` (Chapter 5) walks to the parent `/tmp` and the target `foo`, then
calls `FsOps::unlink(parent_id, "foo", target_id=42)`.

Per the Chapter 3 contract, tmpfs's `unlink`:

- removes `"foo"` from `/tmp`'s directory `BTreeMap`;
- drops inode 42's link count (`nlink` 1 → 0);
- **does not** destroy inode 42 — "open files, live RNodes, and page-cache state
  may still address `target`."

The VFS side removes the cached `DEntry` "foo" from `/tmp`'s children, dropping
the **name edge's `Cap<RNode>`**. Now:

```
   process.fds[fd] ─► Cap<OpenFile> ──► Cap<RNode>(42) ──► PageBacked{ pc } ──► Cap<PageContainer>
                          (open edge)                                            (bytes still here)

   /tmp ─► (no "foo" entry)            ← name edge CUT
```

This is the moment the whole series is about. In Chapter 2's terms:

```
   Inode(42).payload_live  ⇔  nlinks > 0  ∨  open_refs > 0
                              = (0 > 0)   ∨  (1 > 0)
                              = false     ∨  true   =  TRUE
```

The `LinkPin` is gone (nlink 0); the `OpenPin` remains (fd still open). The
disjunction is still true, so the payload stays. And the `RNode` *identity* is
still pinned by the open edge's `Cap<RNode>`. **Identity outlived its name;
payload outlived its last link.** No flag, no special "orphan inode" list — just
one of two reference chains being cut while the other holds, exactly as the
types guarantee.

## [D] `read(fd, buf, 100)` — still works

The file has no name now. `ls /tmp` would not show it. But `fd` still indexes
the same `Cap<OpenFile>`, which still holds `Cap<RNode>(42)`, whose `backing`
still points at the live `PageContainer`. `sys_read` runs exactly as in [B] —
reads the same bytes (from offset 100, or wherever the offset sits). The kernel
never had to special-case "reading an unlinked file," because from the open
chain's point of view *nothing changed*. The name edge was never how reads got
to the bytes.

> **Traditional VFS vs txKernel.** Linux makes this work by keeping the
> `struct inode` alive on the strength of `i_count > 0` even though `i_nlink ==
> 0`, and reclaiming it in `iput` when the count hits zero. txKernel makes it
> work because the open edge holds a `Cap<RNode>` (identity) and the page
> container is pinned by an open-contribution pin (payload) — two explicit,
> separately-typed retentions, neither of which the `unlink` touched. Same
> observable POSIX behaviour; in txKernel it is the *default* consequence of the
> reference types rather than a refcount kept deliberately non-zero.

## [E] `close(fd)` — identity and payload rejoin and reclaim

`sys_close(fd)` is `set_fd(fd, None)`: the `Cap<OpenFile>` leaves the table.

- That was the last `Cap<OpenFile>`, so the `OpenFile` reclaims, dropping the
  **open edge's `Cap<RNode>`**.
- That was the last `Cap<RNode>(42)` — no names, no other opens — so the `RNode`
  identity becomes reclaimable.
- The payload disjunction is now `nlinks(0) > 0 ∨ open_refs(0) > 0 = false`. The
  `OpenPin` drops; the `PageContainer` becomes reclaimable.
- With no live payload references remaining, the VFS calls
  `FsOps::destroy_inode(42)` (Chapter 3) — tmpfs removes inode 42 from its table
  and the pages are freed.

```
   process.fds[fd] ─► (empty)

   OpenFile dropped → Cap<RNode>(42) dropped → RNode(42) reclaims
                                             → PageContainer reclaims
                                             → FsOps::destroy_inode(42)
```

Identity and payload, which parted ways at [C], are both gone now — at the
moment the *last* of the two predicates went false. The reclamation is driven by
the reference types reaching zero, not by a sweep or a flag check. EBR adds its
usual deferral (the slot frees two epochs after the last guard that could have
seen it quiesces), so even a walker that observed inode 42 microseconds before
the close is safe.

## The mapping table, now populated

Every row of the README's table appeared in this one trace:

| Traditional VFS | txKernel | Where it showed up |
|---|---|---|
| `struct inode` | `RNode` (identity) + `RNodeBacking` | [A] materialise, [C] survives unlink |
| inode number | `FsObjectId(42)` | [A] `lookup` result |
| `stat` fields | `InodeMeta` | [A] `load_inode_meta` |
| `struct dentry` + dcache | `DEntry` "foo", weak in `/tmp` children | [A] cached, [C] removed |
| `struct file` | `Cap<OpenFile>` | [A] built, [E] dropped |
| fd table | `process.fds` | [A] install, [E] clear |
| superblock + `vfsmount` | `MountIdentity` + `MountPayload` | [A] mount crossing |
| `inode_operations` + `file_operations` | `FsOps` | [A] `lookup`, [C] `unlink`, [E] `destroy_inode` |
| `address_space_ops` / page cache | `FsPageBacking` + `PageContainer` | [B]/[D] reads |
| `link_path_walk` | walker `kernel_step` | [A] |
| `do_sys_open` | `sys_openat` / `step_open` | [A] |
| `permission()` | `Credential` + predicates | [A] search + open checks |

## The one idea, restated

A file is two things with two lifetimes: an *identity* (which name, which inode,
what descriptors point at it) and a *payload* (the bytes, the buffer, the
device). A traditional VFS fuses them and manages the divergence with one
overloaded refcount and a set of special cases — orphan inodes, `i_count` vs
`i_nlink`, forced-unmount bookkeeping. txKernel splits them at the type level:
`Cap` pins identity, operational evidence pins payload, `Weak` is a stale hint,
`IdentRef` is a guarded glance. Every hard VFS problem in this tutorial —
unlinked-but-open files, force-unmount, refcount-free lookup, clean race
degradation — is the same problem (identity and payload want to die at different
times) and the same solution (so let them, and let the types say which one you
hold).

That is the txKernel VFS. The shape is the VFS you already knew; the difference
is one principled split, applied everywhere it matters.

## Source anchors

- Open path: `crates/tx-shims/src/linux_syscall/fs_basic.rs:718` (`sys_openat`)
- Read path: `crates/tx-shims/src/linux_syscall/io.rs:2179` (`sys_read`)
- Unlink op: `crates/tx-subsystems/src/vfs/composite.rs:298` (`UnlinkOp`)
- `unlink` "do not destroy inode" contract: `crates/tx-subsystems/src/vfs/execution.rs:96`
- Read dispatch: `crates/tx-subsystems/src/vfs/execution.rs:334` (`step_read`)
- Compound payload predicate (`nlinks > 0 ∨ open_refs > 0`): `docs/design/00_meta-framework/object_model_v2.md` §3.3
- Close: `crates/tx-shims/src/linux_syscall/fs_basic.rs:1176` (`sys_close`)
