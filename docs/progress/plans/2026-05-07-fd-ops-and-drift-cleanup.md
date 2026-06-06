# fd ops slice + drift cleanup

**Status:** complete (2026-05-07). Decision note:
[`docs/progress/decisions/2026-05-07-fd-ops-and-drift-cleanup.md`](../decisions/2026-05-07-fd-ops-and-drift-cleanup.md).
4 fd-ops waves + drift cleanup chore + CSPRNG prerequisite chore
shipped on `feat/fd-ops`. Workspace 984 / 984 lib+tests green.

Closes the biggest remaining unblock for booting a real shell — the
trio's fixed 8-slot `[Option<Cap<OpenFile>>; 8]` fd table plus the
absence of `NR_OPENAT`, `NR_DUP*`, `NR_PIPE2`, `NR_CLOSE`, `NR_LSEEK`
prevents anything beyond the bake-in fixture from doing meaningful
file I/O or shell redirection. Companion audit:
[`docs/progress/research/2026-05-07-interface-drift-audit.md`](../research/2026-05-07-interface-drift-audit.md).

## Goal

Demonstrable: a static-musl-shape fixture binary that
`openat(AT_FDCWD, "/tmp/x", O_RDWR|O_CREAT, 0644)`s a tmpfs file,
`dup`s its fd, writes through both, `lseek`s back, reads, closes,
and exits with status 0. Plus `pipe2(O_CLOEXEC)` wired so a future
fork+exec shell can pipe between commands. Layer A smoke (front-end
exec → first instruction at entry) is sufficient; Layer B (full
reactor-driven round-trip) deferred per established slice norm.

LTP unlock estimate: ~30–50 tests (`open*`, `close*`, `dup*`,
`pipe*`, `lseek*`, plus shell-style fd-redirect tests scattered
across `fs/` and `pty/`).

## Doc anchors

- `docs/design/05_filesystem/VFS_CHECKS_V2.1.md` — walker contract;
  `step_open` at terminal-component is already DAC-aware.
- `docs/design/05_filesystem/MOUNT_v1.md` — mount registry already
  wired (pre-ELF Phase 6).
- `docs/design/04_process-signals/PROCESS_v1.md:349-365` — Frame
  declares `fd_table: Shared<FdTable>` (current code has flat
  `fds: SpinMutex<[Option<Cap<OpenFile>>; 8]>`; spec ratification
  via Tier-1 #6 is out of scope — fd-ops keeps the flat shape and
  grows the slot).
- `docs/design/02_execution/EXEC_v1.md:966-978` — close-on-exec is
  already plumbed (`step_close_cloexec_fds`).

## Part 1 — fd table growth

**Pick: BTreeMap<u32, Cap<OpenFile>>** over a larger fixed array.

Trade-offs:

- **Lookup cost.** BTreeMap is O(log n); array is O(1). Real
  process fd counts are 3–30, so log_2(30) ≈ 5 vs 1; not a hot
  path. `step_write` does a single fd lookup per syscall.
- **Alloc cost.** Array stays in the payload allocation; BTreeMap
  allocates per-insert. fd churn happens at process startup
  (open + dup + close on the way to exec); a pool can amortise
  if it becomes a hotspot.
- **Sparse fds.** Linux allows `dup3(0, 100, 0)` then `close(0)`,
  leaving fd 100 isolated. Fixed array waste grows with max
  observed fd. BTreeMap memory is exact.
- **`fd_cloexec` width.** Today `AtomicU32`. With BTreeMap, switch
  to a sparse `BTreeMap<u32, ()>` (CLOEXEC set) **or** lift the
  bit into `OpenFile.cloexec: bool` (already exists in
  `OpenFileFlags::cloexec`). **Recommended:** drop `fd_cloexec`
  entirely; consult `OpenFile.flags.cloexec` at exec time. Saves
  the bitmap-vs-fd-table consistency burden.
- **Serialisation across fork.** `step_fork`'s `snapshot_fds()`
  (`process/structure.rs:736`) returns a `[Option<Cap<OpenFile>>;
  8]`. Migrate to `BTreeMap<u32, Cap<OpenFile>>::clone()` (each
  `Cap<OpenFile>` clone is the existing share-by-clone semantic).

**Affected sites:**

- `crates/tx-subsystems/src/process/structure.rs`: change `fds`
  field type, `fd()` / `set_fd()` accessors (4 methods),
  `snapshot_fds()`, `fd_cloexec_word()` removal,
  `set_fd_cloexec()`/`fd_cloexec_get()` either drop or back via
  `OpenFile.flags`. ~80 LOC churn.
- `crates/tx-subsystems/src/process/execution.rs::step_fork`:
  `snapshot_fds` call site → `clone()` over BTreeMap. ~5 LOC.
- `crates/tx-subsystems/src/process/execution.rs::step_close_cloexec_fds`
  (if pre-existing): walk BTreeMap, drop entries with
  `flags.cloexec`. ~15 LOC.
- All `nth_thread`-style tests that poke `fds` via direct array
  index in `process/tests.rs`, `signal/tests.rs`, `walker/tests.rs`,
  `init/tests.rs`. ~40 test sites; each a 1-line change to
  `process.set_fd(idx, Some(cap))`. ~50 LOC across all tests.
- `crates/tx-shims/src/linux_syscall/mod.rs`: every fd-resolving
  arm (`sys_write`, `sys_read`, `sys_brk`'s no-op-on-fd path) reads
  via `process.fd(idx)`; that accessor's signature stays the
  same. Zero churn.

Total Part 1: ~150 LOC.

**Open question (Q1):** Drop `fd_cloexec: AtomicU32` field and
fold into `OpenFile.flags.cloexec`? Pro: single source of truth;
Con: `dup2`/`dup3` semantics carry CLOEXEC differently (DUP2
clears CLOEXEC; DUP3 with `O_CLOEXEC` sets it on the dup'd fd
only) — a per-fd bit is conceptually cleaner. **Default
recommendation:** keep `fd_cloexec` as a sparse `BTreeMap<u32,
()>` for clean dup2/dup3 semantics.

## Part 2 — NR_OPENAT (RV64 generic = 56)

`sys_open` does **not** exist on RV64 generic ABI; only `openat`.
Add `NR_OPENAT = 56` to `tx-shims/src/linux_syscall/numbers.rs`.

`async fn sys_openat(dirfd: i32, path_uaddr: u64, flags: u32, mode:
u32, ctx) -> SyscallResult`:

1. Decode dirfd (AT_FDCWD = -100, supported only); other dirfd → -EBADF.
2. `read_user_cstr(path_uaddr, EXECVE_PATH_MAX)` — reuse the helper
   landed in elf-loader Wave 4.
3. Decode flag bits: `O_RDONLY=0`, `O_WRONLY=1`, `O_RDWR=2`,
   `O_CREAT=0o100`, `O_EXCL=0o200`, `O_TRUNC=0o1000`,
   `O_APPEND=0o2000`, `O_NONBLOCK=0o4000`, `O_DIRECTORY=0o200000`,
   `O_CLOEXEC=0o2000000`. Compose `OpenFileFlags { read, write,
   append, cloexec, ... }`.
4. Cred from `ctx.walker_cred()` (DAC+setuid Wave 2).
5. Walker resolves: `step_open(process.cwd().ok_or(-ENOENT)?, path,
   flags, mode, &cred, &guard).await`. `step_open`
   (`vfs/walker.rs:125`) already validates DAC R/W.
6. **`O_CREAT` path:** if `step_open` returns `ENOENT` and
   `O_CREAT` is set, walk to parent dir, call
   `fs_ops.create_inode(parent, name, mode, &cred, &guard)`,
   re-run `step_open`. This is new — current `step_open` is
   resolve-only.
7. Allocate fd via fd-table grow helper (next-free or
   `dup3`-target fd). Insert `Cap<OpenFile>`. If
   `flags & O_CLOEXEC`, set CLOEXEC bit (or rely on
   `OpenFile.flags.cloexec`).
8. Return fd as i64.

Errno mapping: same as `step_open` (ENOENT, ENOTDIR, EACCES, ELOOP,
ENAMETOOLONG, EMFILE).

~120 LOC (~80 syscall arm + ~40 helper for create-on-open path).

## Part 3 — NR_CLOSE (RV64 generic = 57)

`fn sys_close(fd: i32, ctx) -> SyscallResult`: bounds-check fd,
remove from BTreeMap (`process.set_fd(fd, None)` returns the old
`Cap<OpenFile>`), drop. Returns 0 on success, -EBADF if slot was
empty. **EBR-deferred drop on the `Cap`.**

~25 LOC.

## Part 4 — NR_DUP / NR_DUP3 (NR_DUP2 absent on RV64 generic)

- `NR_DUP = 23`: `sys_dup(oldfd)`. Walks BTreeMap for first free
  fd ≥ 0; clones `Cap<OpenFile>` into the new slot; CLOEXEC
  cleared on the new fd.
- `NR_DUP3 = 24`: `sys_dup3(oldfd, newfd, flags)`. If `oldfd ==
  newfd`, return `-EINVAL` (Linux semantic). Closes existing
  `newfd` (if any), inserts clone at `newfd`. If `flags &
  O_CLOEXEC`, sets CLOEXEC on the new fd.

`NR_DUP2` (`oldfd, newfd`) absent on RV64 generic; musl emits
`dup3(oldfd, newfd, 0)` for `dup2` shape.

~80 LOC for both arms.

## Part 5 — NR_PIPE2 (RV64 generic = 59)

The biggest sub-part. New subsystem: `tx_subsystems::pipe`.

- New `crates/tx-subsystems/src/pipe.rs`:
  - `pub struct PipeIdentity` — identity-only (no payload-cap
    distinction needed for v1; PIPE_BUF = 4 KiB ring).
  - `pub struct PipePayload { ring: SpinMutex<RingBuffer<PIPE_BUF>>,
    reader_count: AtomicU32, writer_count: AtomicU32, wait_channel:
    Channel, wait_carrier_id: u64 }`.
  - `pub fn step_pipe2(flags) -> StepOutcome<(Cap<OpenFile>,
    Cap<OpenFile>)>` — allocate ring, build reader-side and
    writer-side `OpenFile`s. The two OpenFiles share the same
    `Cap<PipePayload>` (struct-backed RNode shape).
  - `RNodeBacking::StructBacked { StructPayload::Pipe(Cap<PipePayload>) }`
    — extend `StructPayload` enum at `vfs/structure.rs`. Routes
    `step_read` / `step_write` through `pipe::step_read` /
    `pipe::step_write` by enum match in
    `OpenFile::step_read`/`step_write` (`vfs/execution.rs:236`).
  - `pipe::step_read(payload, out, guard)`: drain ring; if empty
    and writer_count == 0, return `Ok(0)` (EOF); if empty and
    O_NONBLOCK, return `EAGAIN`; if empty and blocking, return
    `Blocked(WaitToken(payload.wait_carrier_id))`.
  - `pipe::step_write(payload, bytes, guard)`: insert into ring;
    if reader_count == 0, deliver SIGPIPE to caller via
    `step_kill_process(...SIGPIPE)` and return `EPIPE`; if ring
    full and O_NONBLOCK, return `EAGAIN`; if full and blocking,
    block on a writer-side wait carrier.

- `sys_pipe2(uaddr_pipefd, flags, ctx)`: validate flags (only
  `O_CLOEXEC | O_NONBLOCK | O_DIRECT` recognised; other bits return
  `EINVAL`; `O_DIRECT` creates Linux packet-mode pipes),
  call `pipe::step_pipe2(flags)`, allocate two fds, write the
  pair to user via `core::ptr::write_volatile` (TODO(phase-userva)
  marker per other arms).

**Open question (Q2):** Pipe blocking semantic on full ring.
Linux blocks writers when ring is full (no SIGPIPE); SIGPIPE only
fires when the reader side is closed. **Default recommendation:**
match Linux exactly. Need a writer-side wait carrier in addition
to the reader-side one. ~30 LOC extra.

~250 LOC: ~150 pipe.rs + ~50 vfs StructPayload extension + ~50
sys_pipe2 arm.

## Part 6 — NR_LSEEK (RV64 generic = 62)

`sys_lseek(fd, offset, whence)`. Whence: `SEEK_SET=0`,
`SEEK_CUR=1`, `SEEK_END=2`. Per-fd offset stored on `OpenFile`:
add `pub offset: AtomicU64` to `OpenFile` struct
(`vfs/structure.rs`).

- TTY / devfs char devices return `ESPIPE`.
- Pipe returns `ESPIPE`.
- PageBacked file: SEEK_SET stores; SEEK_CUR adds; SEEK_END reads
  the file's current size from the inode's PageContainer length
  and adds offset.

Existing `step_read` / `step_write` at `vfs/execution.rs` need to
consume `OpenFile.offset.fetch_add(read_len)` rather than starting
from offset 0. ~50 LOC for the read/write update + 40 LOC for the
syscall arm.

## Part 7 — NR_GETDENTS64 (RV64 generic = 61) — **defer**

Tractable but grows scope. The `FsOps::readdir` cursor surface
exists at `vfs/execution.rs:97-102`; tmpfs has the override.
Materialising Linux's `struct linux_dirent64` { d_ino, d_off,
d_reclen, d_type, d_name } and tracking the per-fd readdir cursor
adds ~150 LOC and complicates fork semantics (cursor copied?).

**Defer to a sibling slice.** No LTP `getdents64` test is in the
day-1 priority list; `ls` works against a future slice.

## Part 8 — End-to-end smoke

**Pick: extend `init_fixture.rs`** rather than another sibling.
The existing fixture is a fork+wait+exit binary; adding an
openat+write+lseek+read+close before the fork is mechanical (each
syscall is `li a7, NR; ecall` plus arg setup). Pin the new bytes
just like the existing fixture pin tests.

Layer A smoke (`boot_smoke_fd_ops_seeds_init_for_openat_at_entry`):
extend the existing `boot_smoke_fork_wait_seeds_init_for_clone_at_entry`
shape; just assert the first syscall in the new sequence is
`li a7, 56` (NR_OPENAT) at the new entry offset.

~120 LOC fixture extension + ~40 LOC smoke.

## Cross-cutting risks

1. **BTreeMap drop semantics on fork.** `step_fork` clones each
   `Cap<OpenFile>` via the existing share-by-clone semantic; the
   ring buffer for pipes is shared via `Cap<PipePayload>` clone
   (refcount increments). Reader-count / writer-count atomic
   tracking must be incremented at fd-table insert and decremented
   at fd-table-slot drop. **Mitigation:** wrap `Cap<OpenFile>`
   with `OnDropGuard` for pipe-backed files? Or: accept that the
   refcount-on-Cap-clone path naturally tracks reader+writer
   counts because each fd-slot holds a Cap, and Cap drop fires
   the OpenFile drop which decrements the ring's count.
2. **Pipe ring-buffer wait-carrier integration.** Reader-side and
   writer-side carriers; `step_kill_process(SIGPIPE)` is already
   in the signal subsystem. Carrier-lifetime cleanup is the same
   pre-existing leak shape as `TtyIdentity` and
   `ProcessPayload.exit_port` — flagged but not fixed in this
   slice.
3. **lseek on tty/devfs returns ESPIPE.** Mechanical match arm in
   `OpenFile::step_lseek` over `RNodeBacking`.
4. **Partial writes on pipe with reader closed return SIGPIPE +
   EPIPE.** Linux signal-then-error sequence. Add to
   `pipe::step_write`'s drop-reader path.
5. **`O_TRUNC` permission check still deferred** (per DAC+setuid
   plan Q6).
6. **`step_open`'s create-on-open path** is new code in this
   slice. The walker's terminal-component check needs to handle
   "parent exists, name doesn't" by routing to
   `fs_ops.create_inode` instead of returning ENOENT. ~30 LOC
   additional walker code.
7. **EBR-deferred drop on `Cap<OpenFile>` close.** `sys_close`
   takes the slot and drops. The Cap goes through normal
   epoch-deferred reclamation; any racing read on the Cap
   completes against the live OpenFile under the same guard.

## Out of scope (deliberately deferred)

- Real per-task AST signals (Tier-1 #9 in the audit).
- Full directory operations beyond `getdents64` (`readdir`,
  `seekdir`, `telldir`).
- `flock` / `fcntl` beyond `F_GETFD` / `F_SETFD` (already in trio).
- `splice` / `sendfile`.
- `mmap` on file fds (still uses `brk`).
- Fixed-fd allocator (e.g. F_DUPFD_CLOEXEC's "next fd ≥ N" semantic
  beyond plain dup3).
- `sendmsg`/`recvmsg` on UNIX sockets (no socket subsystem yet).
- Named pipes (FIFO inodes; tmpfs would need a new
  `InodeKind::Fifo` materialise_rnode override).
- `O_DIRECT`/`O_SYNC`/`O_DSYNC` semantics.

## Phasing

Estimate **6 PRs** (~1100 LOC total):

1. **Wave 1 — fd table growth.** Migrate `fds:
   BTreeMap<u32, Cap<OpenFile>>`; update `set_fd`/`fd`/
   `snapshot_fds`; tests; `step_close_cloexec_fds`. Smaller — no
   syscall arms. Build smoke: existing trio + fork tests still
   green. ~150 LOC.
2. **Wave 2 — NR_OPENAT + NR_CLOSE.** New numbers, syscall arms,
   walker create-on-open extension. ~170 LOC.
3. **Wave 3 — NR_LSEEK + per-fd offset on OpenFile.** Threads
   through `step_read`/`step_write`. ~90 LOC.
4. **Wave 4 — NR_DUP + NR_DUP3.** Mechanical given fd-table
   accessors from Wave 1. ~80 LOC.
5. **Wave 5 — NR_PIPE2 + tx_subsystems::pipe.** The biggest
   wave. New subsystem + StructPayload extension + sys arm.
   ~250 LOC.
6. **Wave 6 — Fixture extension + Layer A smoke.** Demonstrable
   end-to-end. ~160 LOC.

## Open questions

**All three defaults confirmed by user 2026-05-07.** The decisions
below are the operative shape; discussion preserved for reviewer
context.

1. **fd-table backing — DECIDED 2026-05-07: `BTreeMap<u32, Cap<OpenFile>>`.**
   Sparse-fd case is real (shells routinely use fd 100+ for
   `>&100`-style redirection); fixed-array would actively break
   them. Per-fd cloexec metadata moves to a sibling
   `BTreeMap<u32, ()>` (or `BTreeSet<u32>`) on `ProcessPayload`
   replacing the `fd_cloexec: AtomicU32` bitmap; the bitmap's fd-31
   ceiling drops with the table size.
2. **Pipe blocking model — DECIDED 2026-05-07: match Linux exactly.**
   Both reader-on-empty and writer-on-full block; SIGPIPE only on
   writer-with-closed-reader. Cost: writer-side wait carrier
   (~30 LOC) on top of the existing ring buffer.
3. **`NR_GETDENTS64` — DECIDED 2026-05-07: defer.** ~150 LOC of
   cursor + d_type marshalling; LTP `ls`-style tests aren't on
   day-1 priority. A future "directory ops" mini-slice owns it.

## Drift cleanup batch (separate small chore)

A batch chore landing alongside the fd-ops slice would close 5
Tier-2 audit items in ~200 LOC:

1. **Move `AtomicSlot<T>` from `tty/structure/identity.rs:72-115`
   to `tx-substrate::sync` or a new `tx-substrate::slot` module.**
   Re-export at substrate crate root. Update the 1 import site
   in `process/structure.rs:39` and the in-tree
   `tty/structure/identity.rs:262`. Audit Tier-2 #1. ~80 LOC
   (mostly file move).
2. **Add `AT_ENTRY` and `AT_BASE` to `AuxvFacts` and the
   stack-build helper** at `tx-scripts/src/process/exec/stack.rs:74-185`.
   Bumps `AUXV_PAIR_COUNT` from 11 to 13. The `image_plan.entry`
   value already exists; `AT_BASE = 0` (no interpreter in v1).
   AT_HWCAP / AT_HWCAP2 / AT_PLATFORM / AT_FLAGS / AT_EXECFN can
   defer per audit Tier-1 #5 (musl tolerates absence); just add
   the two that matter for static-EXEC. Audit Tier-1 #5
   (partial). ~50 LOC.
3. **Amend `docs/design/01_substrate/HAL_v1.md` §13** with the
   `IrqIf::UART_IRQ` const and the explicit-registration choice
   (delete the linkme example, replace with the
   `register_irq_handler` shape). Audit Tier-2 #6, Tier-1 #4.
   ~40 LOC of doc edits.
4. **Amend `docs/design/01_substrate/HAL_v1.md`** with a new
   section for `EntropyIf` (mirroring §13.1's IRQ trait surface
   shape). Audit Tier-1 #3. ~30 LOC of doc edits.
5. **Amend `docs/design/04_process-signals/PROCESS_v1.md` §2.2**
   with a v2 amendment ratifying the flat `ProcessPayload` shape
   that has carried through 5 slices, listing `Frame` /
   `Shared<T>` / `ProcessPolicy` as deferred-to-v2. Audit Tier-1
   #6. ~40 LOC of doc edits.

The chore is doc-heavy + one mechanical code move; can ship before
fd-ops Wave 1 (the AtomicSlot move makes Wave 1's fd-table
migration cleaner if landed first). Ship as a separate PR sized
~200 LOC; not part of the 6 fd-ops waves.
