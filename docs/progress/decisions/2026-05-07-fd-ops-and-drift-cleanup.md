# fd-ops slice + drift cleanup chore

**Date:** 2026-05-07
**Branch:** `feat/fd-ops`
**Plan:** [`docs/progress/plans/2026-05-07-fd-ops-and-drift-cleanup.md`](../plans/2026-05-07-fd-ops-and-drift-cleanup.md)
**Audit:** [`docs/progress/research/2026-05-07-interface-drift-audit.md`](../research/2026-05-07-interface-drift-audit.md)
**Status:** Complete (4 fd-ops waves + drift cleanup chore + CSPRNG
chore prerequisite). 7 commits on top of dac-and-setuid. Workspace
984 / 984 lib+tests green (`--test-threads=1`); `cargo build
--workspace --lib --tests` clean.

## Goal

Closes the biggest day-1 fd-ops blocker before booting a real shell —
the trio's fixed 8-slot `[Option<Cap<OpenFile>>; 8]` fd table plus
the absence of `NR_OPENAT`, `NR_DUP*`, `NR_PIPE2`, `NR_CLOSE`,
`NR_LSEEK` prevented anything beyond the bake-in fixture from doing
meaningful file I/O or shell redirection.

LTP unlock estimate: ~30–50 tests across `open*`, `close*`, `dup*`,
`pipe*`, `lseek*`, plus shell-style fd-redirect tests scattered
across `fs/` and `pty/`.

## What landed

### Prerequisite chore — CSPRNG via HAL EntropyIf (commit `f2d8a67`)

Vendored xorshift64 default impl on a new `EntropyIf` trait added
to the `TxPlatform` super-trait. Per-exec `AT_RANDOM` fill at stack
build time. RV64 platforms override with `rdtime`-mixed seeding.
Added `EntropyIf for TestPlatform {}` to all 5 tx-substrate
integration test platforms. Audit Tier-1 #3 closed.

### Drift cleanup chore — 5 audit items (commit `0516911`)

A doc-heavy + one mechanical move chore that lands before fd-ops
Wave 1 because the `AtomicSlot` move makes the upcoming fd-table
BTreeMap migration cleaner, and `AT_ENTRY` / `AT_BASE` should be
added once not again:

1. `AtomicSlot<T>` moved from `tty/structure/identity.rs` to
   `crates/tx-substrate/src/slot.rs` and re-exported at
   `tx_substrate::AtomicSlot`. Audit Tier-2 #1 closed.
2. `AT_ENTRY = 9` and `AT_BASE = 7` added to `AuxvFacts` and the
   stack builder; `AUXV_PAIR_COUNT` 11 → 13; total auxv contribution
   176 → 208 bytes. Audit Tier-1 #5 (partial) closed.
3. `HAL_v1.md` §13 + §13.2.1: explicit `register_irq_handler` shape
   replacing the linkme-distributed-slice example; 7-point case
   against linkme. Audit Tier-2 #6 + Tier-1 #4 closed.
4. `HAL_v1.md` §13A: new section documenting `EntropyIf` trait
   surface. Audit Tier-1 #3 closed.
5. `PROCESS_v1.md` §2.2.1 v2 amendment: ratifies the flat
   `ProcessPayload` shape that 7 commits built. Audit Tier-1 #6 closed.

### Wave 1 — fd-table BTreeMap migration (commit `203e0fe`)

`ProcessPayload.fds: SpinMutex<[Option<Cap<OpenFile>>; 8]>` →
`SpinMutex<BTreeMap<u32, Cap<OpenFile>>>`. `fd_cloexec: AtomicU32`
(fd-31 ceiling) → `SpinMutex<BTreeSet<u32>>`. `FD_TABLE_SIZE` const
dropped.

New `ProcessIdentity` accessors: `allocate_fd()`,
`allocate_fd_at_least(min)`, `next_fd_above(min)`, `install_fd(fd,
file)`, `fd_cloexec_snapshot()`, `clear_fd_cloexec()`. Existing
`fd(idx)` accessor signature preserved — every `sys_write` /
`sys_read` / `sys_brk` arm reads via the same path. ~50 LOC test
churn across `process/tests.rs`, `signal/tests.rs`, `walker/tests.rs`,
`init/tests.rs` (every `set_fd(idx, Some(cap))` call site).

### Wave 2 — NR_OPENAT + NR_CLOSE + NR_DUP + NR_DUP3 (commit `302bab9`)

Bundles four file-descriptor syscall arms (the plan's Waves 2 and 4
folded into a single commit — arms share helpers and build together).
RV64 generic ABI numbers: `NR_OPENAT = 56`, `NR_CLOSE = 57`,
`NR_DUP = 23`, `NR_DUP3 = 24`. Note `NR_DUP2` absent on RV64
generic; musl emits `dup3(oldfd, newfd, 0)` for `dup2` shape.

Open flag constants added to `linux_syscall/numbers.rs`: `O_RDONLY`,
`O_WRONLY`, `O_RDWR`, `O_ACCMODE`, `O_CREAT`, `O_EXCL`, `O_TRUNC`,
`O_APPEND`, `O_NONBLOCK`. Errno value constants: `ENOENT_VALUE = 2`,
`EEXIST_VALUE = 17`, `EISDIR_VALUE = 21`.

`sys_openat` handles `O_CREAT + O_EXCL` via the syscall-arm-level
`create_then_walk` helper (not inside `step_open` — that step is
resolve-only and doesn't accept O_CREAT bits). `O_CLOEXEC` honored
by setting the cloexec bit on insert. `sys_dup3` rejects same-fd
with `-EINVAL` (Linux dup3 quirk; differs from dup2). All four
arms route through Wave 1's BTreeMap surface.

### Wave 3 — NR_PIPE2 + tx_subsystems::pipe (commit `28b21f2`)

The biggest single piece in the slice — a new ~650-LOC subsystem.
`NR_PIPE2 = 59` syscall arm wired against a new
`tx_subsystems::pipe` module:

- `PipePayload`: 4 KiB ring + reader/writer atomic counts +
  reader-side and writer-side wait carriers (one Channel each,
  registered with `wait_carrier`). Drop releases both carriers.
- `step_pipe2(PipeFlags) -> (Cap<OpenFile>, Cap<OpenFile>)`: builds
  two RNodes (one per side) sharing one `Cap<PipePayload>`. Each
  side's RNode carries `StructPayload::Pipe { payload, side:
  Reader|Writer }`.
- `step_read`: empty + writer alive → Blocked(reader carrier);
  empty + writer closed → Done(0) (EOF). Honors O_NONBLOCK
  per-OpenFile (→ EAGAIN).
- `step_write`: reader closed → `Err(EPIPE)` (caller-arm
  delivers SIGPIPE before returning -EPIPE); full + reader alive
  → Blocked(writer carrier). Honors O_NONBLOCK.

Three new `Errno` variants: `EAGAIN`, `EBADF`, `EPIPE`.
`OpenFileFlags` grows a `nonblocking: bool` field threaded through
all initialiser sites. `sys_write` arm grows a SIGPIPE-on-EPIPE
delivery (the pipe step_write can't deliver SIGPIPE itself — no
process Cap; the syscall arm is the right boundary).

**Q2 DECIDED 2026-05-07** (locked in plan): blocking model matches
Linux exactly. Reader-on-empty blocks; writer-on-full blocks;
SIGPIPE only on writer-with-closed-reader.

### Wave 4 — NR_LSEEK + per-fd offset (commit `bd0e9ea`)

`NR_LSEEK = 62` with SEEK_SET/SEEK_CUR/SEEK_END. `OpenFile.offset:
u64` → `AtomicU64`; `set_offset` now takes `&self` (Release store);
new `advance_offset(delta)` wraps `fetch_add`. `Cap<OpenFile>` clone
now correctly shares the offset cell across `dup`/`fork` (matches
Linux's "shared file description" rule — falls out for free from
the AtomicU64 living inside the Cap'd entity). `Errno::ESPIPE`
added.

`OpenFile::step_lseek` dispatches: PageBacked uses
`PageContainer::size_bytes()` for SEEK_END; TTY/CharDevice/Pipe
return ESPIPE; Directory returns EISDIR; Symlink/Projected return
ENOSYS; negative result and overflow return EINVAL. `step_read` /
`step_write` / `step_range` in `page_backed.rs` and
`page_backed/user_buffer.rs` converted from `&mut OpenFile` to
`&OpenFile`.

End-to-end smoke: sibling `init_lseek_fixture.rs` (228-byte RV64
ELF for `openat → write → lseek → read → close → exit_group`).
Layer A only — `#[cfg(test)] mod`-only, never wired into the
bootstrap path. **Plan Part 8 deviation:** plan said "extend
init_fixture.rs" but DAC slice already proved the sibling pattern
works (init_setuid_fixture.rs). Wave 4 follows that precedent — each
fd-ops / DAC / fork slice now owns its own pinned ABI rather than
sharing a brittle multi-purpose fixture.

## Decisions locked

- **Q1 DECIDED 2026-05-07:** fd-table is `BTreeMap<u32, Cap<OpenFile>>`,
  not a larger fixed array. Sparse-fd case is real (shells use fd
  100+ for `>&100`-style redirection); fixed-array would actively
  break them. Per-fd cloexec metadata moved to a sibling
  `BTreeSet<u32>` on ProcessPayload, replacing the `AtomicU32`
  bitmap.
- **Q2 DECIDED 2026-05-07:** pipe blocking model matches Linux
  exactly. Both reader-on-empty and writer-on-full block; SIGPIPE
  only on writer-with-closed-reader. Cost was a writer-side wait
  carrier (~30 LOC) on top of the existing reader-side carrier.
- **Q3 DECIDED 2026-05-07:** NR_GETDENTS64 deferred to a sibling
  "directory ops" slice. ~150 LOC of cursor + d_type marshalling;
  LTP `ls`-style tests aren't on day-1 priority.

## Verification

- `cargo build --workspace --lib --tests`: clean (no warnings).
- `cargo test --workspace --lib --tests -- --test-threads=1`: 984
  passed / 0 failed.
- Per-crate test deltas across the slice:
  - tx-substrate: integration suites preserved (no LOC changes
    after the AtomicSlot move).
  - tx-subsystems: 405 → 420 (+15 across the BTreeMap accessors
    and the new pipe::tests module).
  - tx-shims: 78 → 109 (+31 across fd_ops_wave1 — wave2 — wave3 —
    wave4 modules: openat / close / dup / dup3 / pipe2 / lseek
    dispatch coverage).
  - tx-kernel: 37 → 43 (+6 init_lseek_fixture::tests byte pins).
  - tx-scripts: 39 → 42 (+3 from drift cleanup auxv pin updates).
- Pre-existing condition: cross-compiled board binaries
  (tx-kernel-*-qemu-virt) fail to link on host without
  cross-toolchains. Verified on Wave 2's commit before any Wave 3
  change — not a regression.
- Pre-existing condition: per-thread test parallelism conflicts on
  global zone/registry state require `--test-threads=1` for green
  tx-subsystems lib runs (carryover from earlier slices).

## Consequences

- **fd table is now sparse.** Shell redirection like `>&100` works.
  fork's `snapshot_fds()` clones the BTreeMap cleanly. CLOEXEC
  semantics across `dup3(O_CLOEXEC)` / `dup2`-shape are now
  per-fd-clean (sibling BTreeSet, not bitmap).
- **`OpenFile::step_*` are all `&self` now.** The atomic offset
  conversion removes the last `&mut OpenFile` site in the workspace
  and makes Cap-shared offset semantics match Linux's "shared file
  description" rule for free. Forked children share offsets with
  parents via the same AtomicU64.
- **First non-trivial new subsystem since the trio.** `tx_subsystems::pipe`
  is the template for any future subsystem that owns a small piece
  of process state (timerfd, eventfd, signalfd would mirror this
  shape).
- **SIGPIPE delivery boundary clarified.** Pipe `step_write` can't
  reach the process Cap; the syscall arm in tx-shims owns the
  SIGPIPE delivery on EPIPE. Documented in the wave-3 commit
  message and in pipe.rs's module header.
- **Plan Part 8 sibling-fixture precedent locked.** Each
  multi-syscall slice (DAC, fd-ops, future signal-fd / timer-fd
  slices) owns its own `init_*_fixture.rs` rather than extending a
  shared fixture. ABI drift is caught per slice without cross-slice
  pin-test churn.

## Alternatives considered

- **Larger fixed fd array (e.g. `[Option<Cap<OpenFile>>; 64]`).**
  Rejected: doesn't scale to shell `>&100`-style redirection;
  `step_fork`'s clone walks the whole array regardless of
  occupancy.
- **Pipe v0 with reader-side carrier only (writer-on-full → EAGAIN
  always, never blocks).** Rejected per Q2: Linux blocks writers
  by default. EAGAIN-only would silently break user code that
  doesn't set O_NONBLOCK. The 30-LOC writer-side carrier cost was
  acceptable.
- **Per-OpenFile offset stored as plain `u64` behind a SpinMutex.**
  Rejected in favour of `AtomicU64`: lseek is hot in shell-style
  workloads, atomic load/store is faster than mutex acquisition,
  and `fetch_add` cleanly expresses the read/write advance.
- **Extend `init_fixture.rs` for the lseek smoke (per plan Part 8).**
  Rejected for the same reason DAC's Wave 5 deviated: the existing
  fixture has 7 byte-pin tests that would break on extension.
  Sibling `init_lseek_fixture.rs` gives independent pinning.
- **Fold pipe lifecycle (`Cap<OpenFile>` Drop → `decr_reader` /
  `decr_writer`) into Wave 3.** Rejected to keep Wave 3 cohesive.
  Tests today exercise the reader/writer count transition by
  calling `decr_reader` / `decr_writer` directly. A future small
  slice (one-step) wires the Drop hook so `close(reader_fd)`
  actually flips `reader_count` to 0 and surfaces SIGPIPE/EPIPE on
  the next writer-side step.

## Follow-ups

- **Pipe lifecycle Drop hook.** Wave 3 carryover. Small, isolated.
- **NR_GETDENTS64 directory-ops mini-slice.** ~150 LOC; cursor +
  d_type marshalling. Per Q3.
- **Pwrite / pread.** Offset-explicit variants — small additions
  once SyscallCtx surfaces are stable.
- **Reactor-driven Layer B end-to-end smoke for `init_lseek_fixture`.**
  Deferred per slice norm; the fixture is `pub` and ready when that
  lane lands.
- **Real per-task AST signal stacks** (audit Tier-1 #9) — already
  out of scope; pipe's SIGPIPE rides the existing process-level
  signal queue.
