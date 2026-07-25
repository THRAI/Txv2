# Part 8 — Special files and the fd zoo

So far "a file" has meant a regular file (page cache) or a directory. But the
`read`/`write`/`close` surface also drives pipes, terminals, devices, and a
whole family of synthetic descriptors — eventfd, timerfd, signalfd, epoll,
pidfd. This chapter is the inventory: how each is a "file" you can `read` without
being an inode you can `stat`, and where each one's logic lives. It is the
breadth chapter — Chapter 6 showed *how* `step_read` dispatches; this shows
*what it dispatches to*.

## Two ways to not be a regular file

There are two distinct mechanisms, and keeping them apart is the key to the
whole zoo:

1. **VFS-rooted special files** — they *are* in the filesystem tree (they have
   an `RNode`, a path, a `stat`), but their `RNodeBacking` is `StructBacked`,
   routing operations to a kernel object instead of a page cache. Pipes (via
   `pipe2`), ttys (`/dev/tty*`), and device nodes (`/dev/null`) are here.

   ```rust
   pub enum StructPayload {                  // vfs/structure.rs:546
       Tty(Cap<TtyIdentity>),
       CharDevice(&'static CharDeviceBinding),
       BlockDevice(&'static BlockDeviceRegistration),
       Pipe { payload: Cap<PipePayload>, side: PipeSide },
       Socket { identity: Cap<SocketIdentity> },
       FsNotify { instance }, NetNamespace { .. }, MountNamespace { .. },
   }
   ```

2. **Non-VFS descriptors** — they have *no* `RNode` and *no* path; they are pure
   kernel objects exposed as fds. The `OpenFile`'s backing is one of the
   non-`Rnode` `OpenFileBacking` variants:

   ```rust
   pub enum OpenFileBacking {                // vfs/structure.rs:1016
       Rnode { rnode: Cap<RNode> },          // ← everything in (1) goes through here
       Eventfd { efd: Cap<EventFd> },
       Timerfd { tfd: Cap<TimerFd> },
       SignalFd { sfd: Cap<SignalFd> },
       Epoll { ep: Cap<Epoll> },
       Pidfd { process: Cap<ProcessIdentity> },
       PosixMq { mq }, Ufd { .. }, AioContext { .. }, IoUring { .. },
       SocketPair { rx, tx }, KernelObject { .. }, MountApi { .. },
   }
   ```

The unifying fact: **every fd is a `Cap<OpenFile>` in the fd table, and every
`read`/`write` enters `OpenFile::step_read`/`step_write`.** That method branches
on backing — first rejecting the non-VFS shapes it can't handle generically,
then matching `RNodeBacking` for the VFS ones (Chapter 6's dispatch table). The
synthetic fds are serviced either by their own syscall handlers (epoll_wait,
timerfd settime) or by dedicated arms that pull the inner cap out via accessors
like `of.eventfd()`, `of.timerfd()`.

The full dispatch fan-out from one `read(fd)` — note the two-level branch
(backing kind, then for `Rnode` the `RNodeBacking`/`StructPayload` tag):

```
   read(fd) → Cap<OpenFile>::step_read
        │
        ▼  match OpenFileBacking
   ┌────────────┬─────────────┬──────────┬─────────┬──────────┬─────────────┐
   │ Rnode      │ Eventfd     │ Timerfd  │ SignalFd│ Epoll    │ Ufd/Aio/    │
   │            │             │          │         │          │ IoUring/…   │
   ▼            ▼             ▼          ▼         ▼          ▼
 match          step_event    step_timer signalfd  step_epoll  EINVAL
 RNodeBacking   fd_read       fd_read    _read     _wait       (no generic
   │            (counter)     (expiry)   (siginfo) (readiness)  read path)
   ▼ match StructPayload / kind
   ┌──────────┬──────────┬────────────┬───────────┬────────────┬──────────┐
   │PageBacked│ Pipe     │ Tty        │ CharDevice │ Projected  │ Directory│
   ▼          ▼          ▼            ▼            ▼            ▼
 page_backed  pipe::     tty::exec::  binding.ops  fs_ops().    EISDIR
 ::step_read  step_read  step_read    .read        step_read_
 (Chapter 7)  (ring)     (line disc.) (/dev/null)  projected
```

Two backings, one fd-table abstraction. The left spine (`Rnode`) is everything
with a path; the right siblings are the inode-free synthetic fds, each reached
by its own accessor and serviced by its own subsystem.

> **Traditional VFS vs txKernel.** Linux makes everything-is-a-file uniform with
> `struct file` + `file_operations`: a pipe, an eventfd, and a regular file all
> have an inode (often an anonymous one from `anon_inode_getfd`) and a `f_op`
> vtable. txKernel splits the difference: VFS-rooted special files reuse the
> `RNode`/`StructBacked` path (they have real inodes), while the synthetic fds
> skip the inode entirely and live as typed `OpenFileBacking` variants. The
> uniformity is at the `OpenFile`/fd-table layer, not forced down to a fake
> inode.

## Pipes

A pipe is one `PipePayload` shared by two `RNode`s — a reader-end and a
writer-end, distinguished by `PipeSide` (`pipe/mod.rs:131,88`):

```rust
pub struct PipePayload {
    ring: SpinMutex<PipeRing>,                  // the byte buffer
    reader_count: AtomicU32, writer_count: AtomicU32,
    reader_wait_source: Arc<WaitSource>, writer_wait_source: Arc<WaitSource>,
    reader_wait_channel: Channel, writer_wait_channel: Channel,   // legacy path
}
pub enum PipeSide { Reader, Writer }
```

`step_read` (`pipe/mod.rs:893`) is a compact statement of pipe semantics:

```rust
fn step_read(payload, out, _guard, nonblocking) -> StepOutcome<usize, ByteProgress> {
    if out.is_empty() { return done_bytes(0); }
    let mut ring = payload.ring.lock();
    if !ring.is_empty() {
        let n = ring.drain_to_slice(out);
        drop(ring);
        notify_writable(&payload.writer_wait_source);   // woke any blocked writer
        return done_bytes(n);
    }
    drop(ring);
    if payload.writer_count.load() == 0 { return done_bytes(0); }  // all writers gone → EOF
    if nonblocking { return eagain(); }
    wait_until_readable(payload.reader_wait_source_id)             // block: yield on readable
}
```

Read it as the four pipe rules:

- **data present** → drain, and wake writers parked on "space available";
- **empty, writers gone** → `Done(0)`, i.e. EOF;
- **empty, nonblocking** → `EAGAIN`;
- **empty, blocking** → yield on the reader wait-source (Chapter 9 covers the
  park/wake mechanics).

`step_write` (`pipe/mod.rs:939`) mirrors it, and shows the `PIPE_BUF` atomicity
rule explicitly:

```rust
fn step_write(payload, bytes, _guard, nonblocking, packet_mode) -> StepOutcome<usize, ByteProgress> {
    if bytes.is_empty() { return done_bytes(0); }
    if payload.reader_count.load(Acquire) == 0 { return epipe(); }   // no readers → EPIPE→SIGPIPE
    let mut ring = payload.ring.lock();
    // atomicity: a ≤PIPE_BUF write must go all-or-nothing; if it can't fit now, wait for room
    if bytes.len() <= PIPE_BUF && !ring.can_write_atomic(bytes.len(), packet_mode) {
        drop(ring);
        if nonblocking { return eagain(); }
        return wait_until_writable(payload.writer_wait_source_id);
    }
    if !ring.is_full() || ring.available_write_capacity() > 0 {
        let copied = ring.fill_from_slice(bytes, packet_mode);       // may be partial for >PIPE_BUF
        if copied > 0 {
            drop(ring);
            notify_readable(&payload.reader_wait_channel, &payload.reader_wait_source);  // wake readers
            return done_bytes(copied);
        }
    }
    drop(ring);
    if nonblocking { return eagain(); }
    wait_until_writable(payload.writer_wait_source_id)
}
```

The `bytes.len() <= PIPE_BUF && !can_write_atomic(..)` guard is the POSIX
guarantee: a small write either lands whole or blocks until it can — it never
interleaves with another writer's bytes. A write *larger* than `PIPE_BUF` skips
that guard and is allowed to fill partially (`copied` < `bytes.len()`), with the
step engine resuming the rest. `packet_mode` (a pipe opened `O_DIRECT`) keeps
message boundaries; it threads through `can_write_atomic`/`fill_from_slice`.

## Terminals (tty)

A tty is the most operation-rich special file: it has a line discipline, a
foreground process group, a window size, and a termios. Its `StructPayload` is
`Tty(Cap<TtyIdentity>)`, and reads/writes/ioctls route into `tty::execution`:

- **`tty::execution::step_read`** applies the line discipline. In *canonical*
  mode it returns whole lines (delimited by newline / EOF), honoring `VMIN`/
  `VTIME` thresholds; in *raw* mode it returns bytes as they arrive. An empty
  input queue yields on the tty's input wait-source. It also enforces job
  control: a background process group reading its controlling tty gets stopped
  (`SIGTTIN`).
- **`tty::execution::step_write`** queues output, fires output readiness, and
  applies output processing.
- **`tty::execution::step_ioctl`** is the control surface, dispatched through a
  typed enum rather than raw request numbers (`vfs/structure.rs:459`):

  ```rust
  pub enum OpenFileIoctl<'a> {
      Tcgets, Tcsets { termios },          // TCGETS/TCSETS — get/set termios
      Tiocgpgrp, Tiocspgrp { new_pgrp },   // TIOCGPGRP/TIOCSPGRP — foreground pgrp
      Tiocgwinsz, Tiocswinsz { winsize },  // TIOCGWINSZ/TIOCSWINSZ — window size (SIGWINCH)
      Tiocsctty, Tiocnotty,                // controlling-terminal bind/release
  }
  pub enum OpenFileIoctlResult { None, Termios(Termios), Pgrp(u32), Winsize(Winsize), SideEffect(IoctlSideEffect) }
  ```

The `SideEffect` result is how an ioctl drives kernel-visible consequences —
changing the foreground pgrp, delivering a signal — back through the syscall
layer. `step_ioctl` rejects non-tty backings with `ENOTTY` (the right errno for
"inappropriate ioctl for device").

## Character devices

A device node (`/dev/null`, `/dev/zero`, `/dev/urandom`) is a name in devfs
whose behaviour is a *driver*, not stored bytes. `StructPayload::CharDevice`
holds a static binding (`device.rs:47,53`):

```rust
pub trait CharDeviceOps: Send + Sync + 'static {
    fn read(&self, out: &mut [u8], guard) -> StepOutcome<usize, ByteProgress>;
    fn write(&self, bytes: &[u8], guard) -> StepOutcome<usize, ByteProgress>;
}
pub struct CharDeviceBinding { pub devt: DevT, pub name: &'static str, pub ops: &'static dyn CharDeviceOps }
pub struct DevT(u64);   // (major << 32) | minor
```

The devfs implementations are tiny and illustrative (`tx-fs/src/devfs/mod.rs`):

| Node | `read` | `write` |
|---|---|---|
| `/dev/null` | `Done(0)` (instant EOF) | `Done(len)` (discard) |
| `/dev/zero` | fill `out` with `0x00` | `Done(len)` (discard) |
| `/dev/urandom` | fill with PRNG bytes | `Done(len)` (discard) |

`OpenFile::step_read` reaching `StructBacked { CharDevice(binding) }` simply
calls `binding.ops.read(out, guard)` (Chapter 6's table). The filesystem owns
the *name and identity*; the device subsystem owns the *behaviour* — the split,
applied to devices.

## The synthetic fds

These have no inode. Each is a small kernel object with its own read/write
semantics, created by a dedicated syscall, and reached through its
`OpenFileBacking` variant. They exist because Linux made each one a file so it
composes with `poll`/`epoll`/`select`.

**eventfd** — a 64-bit counter (`eventfd/mod.rs:121,161`). Read drains, write
adds, both via a CAS loop so concurrent eventfd users never lose an update:

```rust
fn step_eventfd_read(efd, out: &mut [u8; 8], nonblocking) -> ByteOutcome {
    if efd.is_semaphore() {
        loop {                                          // semaphore: hand out 1 at a time
            let current = efd.counter.load(Acquire);
            if current == 0 {
                if nonblocking { return eagain(); }
                return wait_until_readable(efd.reader_source_id);
            }
            if efd.counter.compare_exchange(current, current - 1, AcqRel, Acquire).is_ok() {
                out.copy_from_slice(&1u64.to_le_bytes());
                if current > EVENTFD_MAX { efd.fire_writable(); }
                return ByteOutcome::done(8);
            }                                           // CAS lost → retry
        }
    }
    let val = efd.counter.swap(0, AcqRel);              // normal: read drains the whole counter
    if val == 0 {
        if nonblocking { return eagain(); }
        return wait_until_readable(efd.reader_source_id);   // block until a write
    }
    out.copy_from_slice(&val.to_le_bytes());
    efd.fire_writable();                                // counter dropped → writers may proceed
    ByteOutcome::done(8)
}

fn step_eventfd_write(efd, val: u64, nonblocking) -> StepOutcome<(), NoProgress> {
    if val == 0 || val == u64::MAX { return Err(EINVAL); }   // reserved values
    loop {
        let current = efd.counter.load(Acquire);
        let max_add = EVENTFD_MAX.saturating_sub(current);
        if val > max_add {                              // would overflow the counter
            if nonblocking { return eagain(); }
            return wait_until_writable(efd.writer_source_id);
        }
        if efd.counter.compare_exchange(current, current + val, AcqRel, Acquire).is_ok() {
            if current == 0 { efd.fire_readable(); }    // 0→positive edge wakes readers
            return done(());
        }                                               // CAS lost → retry
    }
}
```

The two halves are symmetric: read blocks (or `EAGAIN`s) when the counter is
`0`, write blocks when adding would exceed `EVENTFD_MAX`; each wakes the other's
wait-source only on the *edge* that matters (`fire_writable` after a drain,
`fire_readable` on the `0→positive` transition). `EFD_SEMAPHORE` changes read
from "drain to 0" to "subtract 1," giving counting-semaphore semantics. Read and
write are always exactly 8 bytes — the `&mut [u8; 8]` type encodes it.

**timerfd** — a counter of expirations (`timerfd/mod.rs:81,363`). `read` returns
the number of times the timer fired since the last read (draining it); zero
expirations blocks or `EAGAIN`s. The deadline/interval live in atomics; the
syscall arm arms the timer against the clock subsystem and the read reports
overruns.

**signalfd** — a queue of pending signals as bytes (`signalfd/mod.rs:105,419`).
`read` drains the process's pending signals that match the fd's `mask` into
128-byte `signalfd_siginfo` records (one per read). It is how a program turns
asynchronous signal delivery into a synchronous, pollable byte stream. The owning
process posting a matching signal fires the fd's wait-source.

**epoll** — a readiness aggregator (`epoll/mod.rs:37,59`):

```rust
pub struct Epoll { fds: SpinMutex<BTreeMap<u32, EpollEntry>>, wait_source: Arc<WaitSource> }
pub struct EpollEntry { fd: u32, interests: u32, source: WaitSourceId, last_ready: u32, disabled: bool }
```

`epoll_ctl` registers a monitored fd plus its interest mask and the *wait-source
of that fd*; `epoll_wait` checks each registered fd's readiness and, if none are
ready, yields on the epoll's own wait-source (`YieldShape::OnEdge`). It is a
fan-in over the same wait-source machinery every blocking file uses — Chapter 9.

**pidfd** — `Pidfd { process: Cap<ProcessIdentity> }`. A handle to a process for
`pidfd_send_signal`/`waitid`; its readiness is process exit. It is not
read/write-able as bytes (rejected `EINVAL`); it exists to make "this specific
process" a race-free, pollable object instead of a reusable pid number.

**socketpair** — `SocketPair { rx, tx }`, two `PipePayload`s wired
head-to-tail, giving a bidirectional `AF_UNIX` stream. Syscall handlers reach
the right end via `of.socketpair_endpoint()` before doing pipe-style I/O.

The remaining variants (`Ufd` userfaultfd, `AioContext`, `IoUring`, `PosixMq`,
`KernelObject`, `MountApi`) are staged subsystems with their own syscall
surfaces; the generic `step_read`/`step_write` rejects them with `EINVAL` so a
stray `read()` on one fails cleanly rather than misdispatching.

## Runtime flag overrides: `fcntl(F_SETFL)`

A subtlety the zoo needs: `O_NONBLOCK` can be flipped *after* open by
`fcntl(F_SETFL)`. `OpenFile` carries the construction-time `flags` plus two
atomic overrides (`vfs/structure.rs:1170`):

```rust
nonblocking_override: AtomicI8,   // -1 unset, 0 false, 1 true
packet_override: AtomicI8,        // pipe packet mode (O_DIRECT on a pipe)

fn flags(&self) -> OpenFileFlags {
    let mut f = self.flags;
    match self.nonblocking_override.load() { 0 => f.nonblocking = false, 1 => f.nonblocking = true, _ => {} }
    match self.packet_override.load()      { 0 => f.packet = false,      1 => f.packet = true,      _ => {} }
    f
}
```

Every `step_read`/`step_write` calls `self.flags()` to get the *current*
blocking mode, so a `fcntl` toggle takes effect on the next I/O without
re-opening — and the `-1` sentinel means "no override, use the open-time value."
The atomics let a concurrent `fcntl` race a `read` without a lock on the hot
path.

## Source anchors

- `StructPayload`: `crates/tx-subsystems/src/vfs/structure.rs:546`
- `OpenFileBacking` variants: `crates/tx-subsystems/src/vfs/structure.rs:1016`
- pipe (`PipePayload`, `PipeSide`, `step_read`/`step_write`, `PIPE_BUF`): `crates/tx-subsystems/src/pipe/mod.rs:131,88,893,939,74`
- tty ioctl enums: `crates/tx-subsystems/src/vfs/structure.rs:459`; tty exec: `crates/tx-subsystems/src/tty/execution/`
- char device (`CharDeviceOps`, `CharDeviceBinding`, `DevT`): `crates/tx-subsystems/src/device.rs:47,53,14`; devfs bindings: `crates/tx-fs/src/devfs/mod.rs:217,229,237,250,324`
- eventfd: `crates/tx-subsystems/src/eventfd/mod.rs:121,161`
- timerfd: `crates/tx-subsystems/src/timerfd/mod.rs:81,363`
- signalfd: `crates/tx-subsystems/src/signalfd/mod.rs:105,419`
- epoll: `crates/tx-subsystems/src/epoll/mod.rs:37,59`; syscalls `crates/tx-shims/src/linux_syscall/epoll.rs`
- fcntl overrides + `flags()`: `crates/tx-subsystems/src/vfs/structure.rs:1170`
- non-VFS accessors (`ufd`, `eventfd`, `timerfd`, `socketpair_endpoint`, …): `crates/tx-subsystems/src/vfs/structure.rs:1519,1644+`
