# Part 6 — `open` / `read` / `write` end to end

The previous chapters built the pieces: entities (1), the split (2), the backend
boundary (3), path resolution (4), mounts (5). This chapter connects them into
the operations a program actually issues — `open`, `read`, `write` — from the fd
integer down to the bytes.

## The fd table

A process's open files live in `ProcessPayload` (note: the *payload* half of the
process split — the fd table dies when the process exits, not when it is
reaped):

```rust
pub(crate) fds: ProcessSpinMutex<BTreeMap<u32, Cap<OpenFile>>>,
```

A sparse map from fd number to `Cap<OpenFile>`. The accessors are what you
expect (`process/structure.rs:514`):

- `fd(idx) -> Option<Cap<OpenFile>>` — look up an fd.
- `allocate_fd() -> u32` — lowest free number (POSIX-lowest-fd).
- `set_fd(idx, Option<Cap<OpenFile>>)` — install or clear.
- `install_fd(...)` — allocate-and-install.

Because the table holds `Cap<OpenFile>` (a clonable identity pin), `dup` and
`fork` are cheap and correct: they clone the `Cap`, so two fds point at *one*
`OpenFile`, sharing its atomic `offset`. That is the POSIX "shared open file
description" semantic, falling straight out of the reference type.

## `open`

`sys_openat` (`fs_basic.rs:718`) is the orchestration; every piece below is from
an earlier chapter:

```
sys_openat(dirfd, path, flags, mode):
  1. copy `path` from user; decode O_* flags into OpenFileFlags
  2. check the fd-count rlimit (EMFILE early)                       ← cheap to fail first
  3. resolve the starting directory:
        absolute path or AT_FDCWD → process.cwd()
        else                      → process.fd(dirfd).opendir_dentry  (Chapter 1)
  4. walk the path  → step_walk / step_open                         (Chapter 4)
        if ENOENT and O_CREAT     → create_inode in the parent, re-walk
  5. if O_TRUNC                   → FsPageBacking::truncate(id, 0)   (Chapter 3)
  6. build the OpenFile:
        OpenFile::new_cap_with_dentry(rnode, flags, dentry) → Cap<OpenFile>
  7. fd = allocate_fd();  set_fd(fd, Some(openfile));
     if O_CLOEXEC → mark cloexec
     return fd
```

Step 4 is the walker; step 6 is the `step_open` terminal from Chapter 4; step 5
reaches into the page-backing trait from Chapter 3. The result is the object
graph from Chapter 1: `fds[fd] → Cap<OpenFile> → Cap<RNode> → backing`.

`O_CREAT` deserves a note: on `ENOENT`, the path is split into parent + basename,
the parent is walked, and `FsOps::create_inode(parent_id, basename, mode, cred)`
makes the inode; then the path is re-walked to materialise a `DEntry` over the
new inode. Creation and lookup are *separate* backend calls — the VFS composes
them.

## `read`

`sys_read` (`io.rs:2179`) resolves the fd, dispatches by backing kind, and for a
VFS file drives `OpenFile::step_read`. The skeleton:

```
sys_read(fd, buf_ptr, len):
  file = resolve_fd(process, fd)?                  // Cap<OpenFile> or EBADF
  match file.backing {                             // non-VFS fds branch first
     EventFd | Timerfd | SignalFd | …  → their own read handlers
     SocketPair | Pidfd | Ufd          → their own handlers / EINVAL
     Rnode { .. }                      → VFS read path ↓
  }
  // VFS read:
  if file is PageBacked → sys_read_pagebacked(file, buf_ptr, len).await  // direct user-buffer
  else:
     let mut staging = Vec::with_capacity(len);
     drive(OpenFileReadOp { file, out: &mut staging, … }).await?         // step-driven
     bootstrap_copy_to_user(buf_ptr, &staging[..total])?
  return total
```

Two read lanes, both ending in `OpenFile::step_read`:

- **Page-backed files** take a fast path (`sys_read_pagebacked`) that reads
  pages directly toward the user buffer, because the page cache already handles
  blocking and EOF.
- **Everything else** reads into a kernel staging buffer through the `drive`
  bridge (Chapter 3), then copies to user with `bootstrap_copy_to_user`. The
  user-copy is a single explicit seam — the backend never touches user memory.

### The dispatch heart: `OpenFile::step_read`

This is where Chapter 1's `RNodeBacking` and Chapter 3's backing gallery cash
out. `step_read` (`execution.rs:334`) checks readability, rejects non-VFS
shapes, then matches on the backing (lightly trimmed):

```rust
fn step_read(&self, out: &mut [u8], guard) -> StepOutcome<usize, ByteProgress> {
    if !self.flags().read { return Err(EINVAL); }       // ① observe: readable?
    match self.rnode().backing() {
        StructBacked { payload } => match payload {       // devfs / pipes / tty
            Tty(tty)            => tty::execution::step_read(tty, out, guard),
            CharDevice(binding) => binding.ops.read(out, guard),       // device driver
            Pipe { payload, side: Reader } =>
                                   pipe::step_read(payload, out, guard, flags.nonblocking),
            Socket { .. } | …   => Err(EINVAL),
        },
        Directory          => Err(EISDIR),
        PageBacked { pc }  => page_backed::step_read_to_kernel(pc, self, out, guard), // tmpfs/ext4
        Symlink { .. }     => Err(ENOSYS),
        Projected { .. }   => {                            // procfs / sysfs
            let mp = self.rnode().containing_mount_weak()?.upgrade(guard)?; // Chapter 5 weak ref
            let n = mp.fs_ops().step_read_projected(id, self.offset(), out, guard)?;
            self.set_offset(self.offset() + n);
            Done(n)
        }
    }
}
```

Read it against the gallery from Chapter 3:

| `RNodeBacking` | `step_read` routes to | Backend (gallery) |
|---|---|---|
| `PageBacked { pc }` | `page_backed::step_read_to_kernel` | tmpfs, ext4/FAT — page cache |
| `Projected { .. }` | `fs_ops().step_read_projected` | procfs/sysfs — synthesised |
| `StructBacked { CharDevice }` | `binding.ops.read` | devfs — device driver |
| `StructBacked { Pipe(Reader) }` | `pipe::step_read` | pipe buffer |
| `StructBacked { Tty }` | `tty::execution::step_read` | tty line discipline |
| `Directory` | `EISDIR` | — |

One method, one `RNode` identity, and the `backing` tag steers each read to the
payload that actually holds (or synthesises) the bytes. The `Projected` arm is
the clearest: it upgrades the `RNode`'s `containing_mount` weak ref to find the
mount's `FsOps`, then calls `step_read_projected` — the procfs file's bytes are
computed right there from live kernel state, and the offset is advanced manually
because there is no stored payload to track position against.

## `write`

`sys_write` (`io.rs:1913`) mirrors `read`: resolve fd, branch on backing, copy
the user buffer into a kernel `Vec` (`bootstrap_copy_from_user`), then drive
`OpenFileWriteOp`, which calls `OpenFile::step_write`. `step_write`
(`execution.rs:591`) dispatches on `RNodeBacking` the same way, with two extra
rules:

- **`O_APPEND`**: before writing, seek the offset to end-of-file, so concurrent
  appenders never clobber each other.
- **writability**: `flags.write` must be set, else `EBADF`/`EINVAL`.

For a page-backed file the write goes through the page cache (dirtying pages,
growing the container via `grow_size_to`); for a pipe it pushes into the ring
buffer and wakes readers; for a char device it calls the driver's `write`.

## Offsets, `lseek`, and the shared description

The `offset: AtomicU64` lives on `OpenFile`, and `lseek` (`step_lseek`,
`execution.rs:469`) updates it:

- `SEEK_SET` → absolute; `SEEK_CUR` → relative; `SEEK_END` → relative to
  `meta().size`.
- Non-seekable backings (pipe, tty, char device) return `ESPIPE`.
- Directories seek the separate `readdir_cursor` used by `getdents64`.

Because the offset is atomic and on the shared `OpenFile`, a `dup`'d fd and its
original advance the *same* position — and a `pread`/`pwrite` (offset passed
explicitly) bypasses it without disturbing it. The split shows up here too: the
offset is *per-open-description* state (it lives on `OpenFile`, the open's
identity), not per-inode state (it is not on `RNode`) and not per-fd state (dup
shares it). Putting it on the right object is what makes all three POSIX
behaviours fall out without special cases.

## `close`

`sys_close(fd)` is just `set_fd(fd, None)` — it removes the `Cap<OpenFile>` from
the table. When that was the last `Cap<OpenFile>`, the `OpenFile` reclaims,
dropping its `Cap<RNode>` (the open edge). If *that* was the last reference to
the `RNode` — no remaining names, no other opens — the inode's payload becomes
reclaimable and the backend's `destroy_inode` runs. That last sentence is the
unlinked-but-open file finally closing. Chapter 10 traces it.

## Source anchors

- fd table + accessors: `crates/tx-subsystems/src/process/structure.rs:1148,514,558,626`
- `sys_openat`: `crates/tx-shims/src/linux_syscall/fs_basic.rs:718`
- `sys_read` / `sys_read_pagebacked`: `crates/tx-shims/src/linux_syscall/io.rs:2179,2104`
- `sys_write`: `crates/tx-shims/src/linux_syscall/io.rs:1913`
- `OpenFile::step_read`: `crates/tx-subsystems/src/vfs/execution.rs:334`
- `OpenFile::step_write`: `crates/tx-subsystems/src/vfs/execution.rs:591`
- `OpenFile::step_lseek`: `crates/tx-subsystems/src/vfs/execution.rs:469`
- `OpenFileReadOp` / `OpenFileWriteOp`: `crates/tx-subsystems/src/vfs/execution.rs`
- `sys_close` / `sys_lseek`: `crates/tx-shims/src/linux_syscall/fs_basic.rs:1176,1374`
