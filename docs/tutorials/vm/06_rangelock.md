# Chapter 6 — RangeLock: coordination without object locks

Linux guards an address space with `mmap_lock` — one reader/writer semaphore for
the whole `mm_struct`. A `mprotect` on one range and a page fault on a completely
unrelated range serialize against each other, because both must take the lock.
Decades of work (per-VMA locks, RCU VMA walks, `mmap_lock` speculation) have gone
into clawing back the concurrency that one big lock throws away.

txKernel does not have that lock. Coordination is `RangeLock`, and its defining
property is that **two operations conflict only if the address ranges they
declare overlap.** This chapter is how that works and why it is enough.

## The mode pair

```rust
// vm/structure/range_lock.rs:26
pub enum LockMode {
    ExclusiveWriter,   // mutates the binding, or invalidates materializations
    Materializer,      // publishes a materialization (a PTE) without mutating bindings
}
```

The two modes map exactly onto the binding/materialization split:

- **`ExclusiveWriter`** is taken by operations that **rewrite the authoritative
  binding** or tear down resident PTEs: `mmap`, `munmap`, `mprotect`, `mremap`,
  the fork parent's snapshot, `exec` teardown.
- **`Materializer`** is taken by the operation that **publishes a derived PTE
  against an existing binding**: the fault handler (Chapter 7), and eager
  prefault.

The conflict rules follow from what each protects:

| | ExclusiveWriter | Materializer |
|---|---|---|
| **ExclusiveWriter** | conflict | conflict |
| **Materializer** | conflict | **no conflict** |

Two `Materializer`s do *not* exclude each other — two threads can fault different
(or even the same) page in the same range concurrently. That is safe because
uniqueness of a published PTE is enforced one layer down (the pmap's idempotent
publish, Chapter 4; the page-cache `install_if_absent`, Chapter 9), not by the
range lock. `RangeLock` only ensures **no binding mutation runs concurrently with
a materialization in the same range** — which is precisely the window where the
justification invariant could be violated.

## The declared-range rule

The conflict domain is the **range the operation declares**, never the incidental
extent of some `VmEntry` it happens to touch. This is a first-class commitment
(`VM_v1_2.md` §3.4), and it is what keeps the lock from degrading into
object-shaped locking.

> Suppose one `VmEntry` covers `[0x0, 0x10000)`. An `mprotect` on `[0x1000,
> 0x2000)` declares its range as exactly `[0x1000, 0x2000)` and takes an
> `ExclusiveWriter` on *that*. A concurrent fault at `0x5000` — inside the same
> `VmEntry`, but outside the declared range — **does not conflict.** It proceeds:
> it observes the entry (pre- or post-split, either is correct, because the
> content at `0x5000` is identical either way) and installs its PTE.

So conflict is defined operationally: two operations conflict iff their declared
ranges overlap. A faulting thread and a `mprotect` thread working different parts
of the same mapping never touch each other. This is the structural answer to
`mmap_lock` contention — the granularity is the operation, not the address space
and not the VMA.

## The structure: a fixed-array interval tree

```rust
// vm/structure/range_lock.rs:114
pub struct RangeLock {
    state: Mutex<RangeLockState>,   // the reservation trees + waiter wiring
    // wait-source / channel fields for blocked-acquire wakeups
}

// vm/structure/range_lock.rs:305
struct RangeLockState {
    active: ReservationIntervalTree<MAX_ACTIVE_RANGES>,        // 16 slots
    pending_writers: ReservationIntervalTree<MAX_PENDING_WRITERS>, // 16 slots
    next_id: u64,
}
```

The reservations live in an **AVL interval tree** augmented with a `max_end` key
for efficient overlap queries (`ReservationIntervalTree`, `range_lock.rs:459`).
It is **fixed-array-backed** (16 active, 16 pending) — no heap allocation beyond
the `RangeLock` itself, sized for the expected contention profile (a handful of
concurrent VM operations per address space). Two trees, not one: `active` holds
granted reservations; `pending_writers` holds writers that are queued, which is
how writer-preference is implemented (below).

The internal `state` mutex is a *spin lock held only for the duration of an
acquire/release bookkeeping operation* — microseconds, never across I/O. It is
not the address-space lock; it protects the interval trees, nothing else.

## Acquiring

The canonical surface returns a step outcome — `Done(guard)` on immediate
acquisition, or a blocked yield carrying a wait token:

```rust
// vm/structure/range_lock.rs:152
pub fn acquire_step(&self, range: UserRange, mode: LockMode)
    -> V3StepOutcome<RangeGuard<'_>, NoProgress>
{
    match self.acquire_step_rich(range, mode) {
        AcquireResult::Acquired(guard) => V3StepOutcome::Done(guard),
        AcquireResult::WouldBlock(blocked) =>
            range_lock_blocked(blocked.wait_token().source_id()),  // yield on a wait source
    }
}
```

`acquire_pair_step` (`range_lock.rs:166`) acquires two ranges atomically — both
or neither — for `mremap`, which must hold the source and destination ranges
together. The underlying `acquire_step_rich` (`range_lock.rs:186`) exposes the
`PendingWriter` carrier used by the writer-preference machinery and tests;
production scripts use the canonical `acquire_step`.

A blocked acquire does **not** return an error and does **not** spin. It yields
on a wait source. When *any* reservation in this `RangeLock` is released, waiters
are woken (`RANGE_LOCK_RELEASE_MASK = 0x1`, `range_lock.rs:23`), and each
re-attempts from scratch. A wake is a *hint that state may have changed*, never a
grant — the woken caller re-runs `acquire_step` and may block again. (This is the
`SIG-1` discipline shared with the rest of the kernel: wake is not truth; fresh
observation authorizes action.)

## Releasing: RAII, and the wake

```rust
// vm/structure/range_lock.rs:70
impl Drop for RangeGuard<'_> {
    fn drop(&mut self) {
        self.lock.release_active(self.id, self.range);   // remove + wake waiters
    }
}
```

The guard is RAII-bound to its stack frame; its lifetime `'a` cannot outlive the
`RangeLock`. Drop fires release unconditionally — on the success path, on an
error-return path, on a panic-unwind. An operation that fails partway (a fault
that hits a permission error after taking `Materializer`) drops its guard through
normal control flow; there is no leak-discipline to get wrong. Release removes the
reservation from the interval tree and fires the release channel so any newly
unblocked waiter re-attempts.

## Writer-preferred, writers FIFO

The fairness policy (`VM_v1_2.md` §3.5) is writer-preferred, with writers served
in FIFO order. It is enforced by the two-tree structure:

```rust
// vm/structure/range_lock.rs:320
fn materializer_blocked(&self, range) -> bool {
    self.active.any_overlap_where(range, |r| r.mode == ExclusiveWriter)
        || self.pending_writers.any_overlap_where(range, |_| true)   // ← key line
}
```

A `Materializer` is blocked not only by an *active* writer in its range but by
any *pending* (queued-but-not-yet-granted) writer too. So once a writer queues,
newly arriving overlapping faults must wait behind it — the writer cannot be
starved by a stream of faults. `writer_blocked` (`range_lock.rs:326`) orders
writers among themselves by a monotonically increasing id (`next_id`), giving
FIFO.

The rationale is the split again: `ExclusiveWriter` mutates *authoritative
bindings*; `Materializer` publishes *derived materializations*. Under heavy fault
pressure (a workload touching fresh anonymous memory fast), a fault-favoring
policy could delay binding mutations indefinitely, because faults can overlap
each other and each wake restarts the race. Prioritizing writers ensures the
*truth* makes progress. Faults are not starved: once the queued writers drain,
faults unblock in FIFO order, bounded by the number of writers ahead of them.

## The cross-async-wait discipline

This is the rule that makes range locks compatible with a blocking, async fault
handler — and it is the single most important operational constraint in the
subsystem:

> **A reservation protects only the synchronous publication or rewrite phase. It
> must not be held across an unbounded asynchronous wait.** (`VM_v1_2.md` §3.6)

If a step yields while preparing a materialization — the fault handler blocking on
disk I/O to fetch a file page, say — it **drops the reservation before yielding**
and **re-acquires and re-observes everything on resume**. Holding a `RangeLock`
across disk I/O would serialize an entire range against every concurrent operation
for tens of milliseconds; the reservation is for synchronous coordination, not for
blocking others while waiting on hardware.

The fault handler (Chapter 7) embodies this literally: take `Materializer`,
observe the recipe, call `materialize_page`; if that blocks, `drop(guard)`, await,
and `continue` the loop from the top — re-acquire (cheap), re-observe the recipe
(which may have changed during the wait), and only then publish. The re-observation
is what closes the TOCTOU window the dropped lock opens: the publication rule
(Chapter 2) is enforced by re-reading the binding *after* re-acquiring, immediately
*before* publishing.

## Where we go from here

You now have the coordination primitive: range-scoped, two modes matching the
binding/materialization split, writer-preferred, dropped across I/O with mandatory
re-observation on resume. Chapter 7 puts it to work in the operation that uses
`Materializer` — the fault handler — and shows the full resolve → materialize →
publish loop, including the read-vs-write fault distinction and the SIGSEGV/SIGBUS
exits.

## Source anchors

- `LockMode`: `crates/tx-subsystems/src/vm/structure/range_lock.rs:26`
- `RangeLock` / `RangeLockState` / interval tree: same file, `:114, 305, 459`; capacities `:295, 296`
- `acquire_step` / `acquire_pair_step` / `acquire_step_rich`: same file, `:152, 166, 186`
- `RangeGuard` + Drop (release + wake): same file, `:64, 70`
- `RANGE_LOCK_RELEASE_MASK`: same file, `:23`
- Writer-preference (`materializer_blocked` / `writer_blocked`): same file, `:320, 326`
- Declared-range rule: `docs/design/03_memory-vm/VM_v1_2.md` §3.4
- Fairness policy: same doc, §3.5
- Cross-async-wait discipline: same doc, §3.6
- Tests exercising the conflict matrix + FIFO: `crates/tx-subsystems/src/vm/tests/range_locks.rs`
