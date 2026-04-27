# BitmapReservation — v1

<!-- txdoc:01-SUBSTRATE-BITMAP-RESERVATION-V1 -->

## Status
<!-- txdoc:BITMAP-RESERVATION-STATUS-1 -->

Draft v1.

This document specifies a small substrate extension to the `AtomicBitmap<N>` primitive: **reservation semantics** with Drop-releases-on-uncommitted rollback. It is a general substrate pattern matching the reserve/commit discipline of SUBSYSTEM_ANATOMY §3. PROCESS pid allocation now uses the higher-level `PidNamespace.numbers` / `AllocIndex` surface from `NAMESPACE_VIEW_v1`; `AllocIndex` may use this bitmap reservation internally.

Companion documents:

- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §3 (five-phase discipline), §4.1 (zone reservation pattern).
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) §9 and [`NAMESPACE_VIEW_v1.md`](../00_meta-framework/NAMESPACE_VIEW_v1.md) (PidNamespace allocation through `AllocIndex`).
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — reservation, step, and publication invariants consumed by this primitive.

### What this document pins
<!-- txdoc:BITMAP-RESERVATION-STATUS-WHAT-THIS-DOCUMENT-PINS-1 -->

- `BitmapReservation<'a>` — the reservation wrapper with Drop-rollback.
- Three new methods on `AtomicBitmap<N>`:
  - `reserve() -> Option<BitmapReservation<'_>>`
  - `reserve_at(i: u32) -> Option<BitmapReservation<'_>>`
  - `try_set(i: u32) -> bool`
- Commit semantics: consume-via-move; uncommitted drop releases the bit.
- Epoch-safety: uncommitted rollback is safe without epoch deferral (no observer sees uncommitted bits).

### What this document does not pin
<!-- txdoc:BITMAP-RESERVATION-STATUS-WHAT-THIS-DOCUMENT-DOES-NOT-PIN-1 -->

- The existing `alloc()` / `free()` primitives of AtomicBitmap (assumed; specified in tx-fnd/bitmap).
- The word-layout of AtomicBitmap internally (u64 words, CAS loops — implementation detail).
- Multi-bit reservations (reserve N bits atomically). Not needed for current consumers; can be added later.

---

## 1. Motivation
<!-- txdoc:BITMAP-RESERVATION-MOTIVATION-1 -->

Step-model steps (STEP-2, SUBSYSTEM_ANATOMY §3) follow a reserve-commit discipline: reservations taken in phase 3 may drop cleanly if a subsequent reservation fails, with no state published. This pattern applies naturally to zone allocators (`ZoneReservation` in `tx-fnd/zone`) and index publications (`Index::commit` with built-in reservation).

For bitmap-based identifier allocation inside a higher-level allocating index, and any other bitmap-allocated namespace, the existing `alloc()`/`free()` API doesn't directly match the pattern:

- `alloc()` returns an index, atomically setting the bit.
- `free()` clears the bit.

Using these directly in a step's phase 3 works, but the caller is responsible for calling `free()` manually if the step aborts after `alloc()` but before commit. This is error-prone.

Three scenarios motivate explicit reservation:

**(a) Reserve-first-free-bit with drop-rollback (case B).**

Step wants a new numeric id. Allocate from bitmap in phase 3. If a later reservation (zone slot, etc.) fails, roll back the allocation. Automating via `BitmapReservation` with Drop-releases-on-uncommitted removes the manual free path.

**(b) Reserve-specific-bit (case A).**

Non-leader exec (Phase 2 PROCESS deferral) wants a specific tid number — the old leader's tid, which was freed at the old leader's reap. The execer must claim *this* bit, not any free bit. Racing callers must fail; only the execer succeeds.

**(c) Raw try-set (primitive for case A).**

Low-level atomic set-specific-bit, returning success/failure based on prior state. Useful for callers that want the atomicity without the RAII reservation wrapper.

---

## 2. API
<!-- txdoc:BITMAP-RESERVATION-API-1 -->

```rust
impl<const N: usize> AtomicBitmap<N> {
    // Existing (assumed, from tx-fnd/bitmap):

    /// Allocate the first free bit. Atomic. Returns the index on success.
    pub fn alloc(&self) -> Option<u32>;

    /// Clear the bit at index i. Atomic.
    pub fn free(&self, i: u32);

    // New:

    /// Atomically set the bit at index i if it is currently clear.
    /// Returns true on success (bit was clear, now set); false on failure
    /// (bit was already set).
    pub fn try_set(&self, i: u32) -> bool;

    /// Reserve the first free bit. Returns a BitmapReservation on success;
    /// None if the bitmap is full.
    ///
    /// The reservation holds the bit set. On commit(), the reservation
    /// consumes itself and the bit stays set (caller is now responsible
    /// for freeing it at the appropriate time). On drop (uncommitted),
    /// the bit is cleared.
    pub fn reserve(&self) -> Option<BitmapReservation<'_, N>>;

    /// Reserve a specific bit. Returns a BitmapReservation on success;
    /// None if the bit was already set.
    ///
    /// Same commit/drop semantics as reserve().
    pub fn reserve_at(&self, i: u32) -> Option<BitmapReservation<'_, N>>;
}

/// A reservation on a bitmap bit. The bit is held set by this reservation
/// until commit() is called (bit stays set indefinitely; caller responsible
/// for eventual free) or the reservation is dropped (bit is cleared).
///
/// !Send and !Sync: single-owner enforced by Rust move semantics.
pub struct BitmapReservation<'a, const N: usize> {
    bitmap: &'a AtomicBitmap<N>,
    index: u32,
    committed: bool,
}

impl<'a, const N: usize> BitmapReservation<'a, N> {
    /// Return the index this reservation holds.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Commit the reservation. Consumes self. The bit stays set; caller
    /// is responsible for freeing it via AtomicBitmap::free at the
    /// appropriate time (e.g., at entity reap).
    ///
    /// Returns the index for convenience.
    pub fn commit(mut self) -> u32 {
        self.committed = true;
        // Drop runs but does nothing because committed is true.
        let idx = self.index;
        // self dropped here, see Drop impl
        idx
    }
}

impl<'a, const N: usize> Drop for BitmapReservation<'a, N> {
    fn drop(&mut self) {
        if !self.committed {
            self.bitmap.free(self.index);
        }
    }
}
```

### 2.1 Send / Sync
<!-- txdoc:BITMAP-RESERVATION-API-SEND-SYNC-1 -->

`BitmapReservation<'a, N>` is `!Send` and `!Sync`. The single-owner invariant (reservation is held by one thread until commit or drop) is enforced by Rust's move semantics. This matches other reservation types in the substrate (ZoneReservation, IndexReservation).

### 2.2 Lifetime
<!-- txdoc:BITMAP-RESERVATION-API-LIFETIME-1 -->

`BitmapReservation<'_, N>` has a lifetime tied to the bitmap reference. This prevents the reservation from outliving the bitmap (which would be a use-after-free). In practice, bitmaps are long-lived kernel objects, so the lifetime is typically the kernel's.

---

## 3. Semantics
<!-- txdoc:BITMAP-RESERVATION-SEMANTICS-1 -->

### 3.1 Committed vs uncommitted
<!-- txdoc:BITMAP-RESERVATION-SEMANTICS-COMMITTED-VS-UNCOMMITTED-1 -->

After `reserve()` or `reserve_at(i)` returns `Some(rsv)`:

- The bit is **set** in the bitmap.
- The reservation is **uncommitted**.
- Other callers trying to reserve this bit via `reserve_at(i)` or trying to `try_set(i)` will fail.
- Other callers doing `alloc()` will not return this bit.

If `rsv.commit()` is called:

- The reservation transitions to **committed**.
- The bit remains set.
- Responsibility for freeing the bit transfers to the caller, to be done via `bitmap.free(idx)` at the appropriate semantic moment (typically at the bound entity's reclamation).

If `rsv` is dropped without commit:

- The Drop impl clears the bit via `free(idx)`.
- Subsequent `alloc()` or `reserve_at(idx)` may now claim this bit.

### 3.2 Epoch safety
<!-- txdoc:BITMAP-RESERVATION-SEMANTICS-EPOCH-SAFETY-1 -->

Unlike zone reservations or published bindings, bitmap reservations do not require epoch deferral for rollback. The reason:

- An uncommitted bit has no observer reachability. No binding, index entry, or DLL names the bit; the bit is not a pointer to anything. It is a claim on a number space.
- Rolling back the claim (clearing the bit) cannot invalidate any observer's reference, because no observer holds a reference to the bit itself.

Contrast with zone reservations: if a zone slot were populated with identifying data before commit and another thread read through a stale reference... but that doesn't apply here either. Zone reservations are rolled back safely when uncommitted because nothing points at them. Bitmap reservations are the same, simpler: even less state to rollback.

Conclusion: Drop-clears-bit is safe without epoch coordination.

### 3.3 Consistency with alloc / free
<!-- txdoc:BITMAP-RESERVATION-SEMANTICS-CONSISTENCY-WITH-ALLOC-FREE-1 -->

The new API extends `alloc`/`free` without replacing them. Simple use cases can still use `alloc`/`free` directly; reservation-based use cases use `reserve` and optionally `commit`. Both coexist in the same bitmap.

Numeric space is shared: a bit `i` cannot be simultaneously allocated (via `alloc`), reserved (via `reserve`), and target of `try_set`. Whichever claim happens first wins.

---

## 4. Usage patterns
<!-- txdoc:BITMAP-RESERVATION-USAGE-PATTERNS-1 -->

### 4.1 Reserve-commit in a step (case B)
<!-- txdoc:BITMAP-RESERVATION-USAGE-PATTERNS-RESERVE-COMMIT-IN-A-STEP-CASE-B-1 -->

```rust
fn alloc_index_reserve_number(index: &AllocIndex<u32, Cap<PidName>>, ...) -> StepOutcome<...> {
    // Phase 3: reserve
    let nr_rsv = index.id_alloc.reserve()
        .ok_or(Errno::EAGAIN)?;   // No free number
    let zone_rsv = zone.reserve_process()?;
    let payload_rsv = zone.reserve_payload()?;
    // ... other reservations
    // If any reservation above fails, earlier ones drop cleanly.
    // nr_rsv's Drop clears the bit if we early-return.

    // Phase 4: commit (all infallible)
    let nr = nr_rsv.commit();              // Bit stays set; number is reserved for index publication
    let proc = zone_rsv.commit(proc_data);
    // ... etc.

    StepOutcome::Done(nr)
}
```

The numeric reservation is held across subsequent fallible operations. If any fails, the reservation drops uncommitted and the number is returned to the pool automatically.

### 4.2 Reserve-specific in non-leader exec (case A, Phase 2)
<!-- txdoc:BITMAP-RESERVATION-USAGE-PATTERNS-RESERVE-SPECIFIC-IN-NON-LEADER-EXEC-CASE-A-PHASE-2-1 -->

```rust
fn step_non_leader_exec_rename_tid(
    pid_numbers: &AllocIndex<u32, Cap<PidName>>,
    old_tid: u32,           // execer's current tid
    new_tid: u32,           // the tid to take over (old leader's, now freed)
) -> Result<IndexReservation<u32>, Errno> {
    // Reserve the specific tid. Fails if already taken (shouldn't happen
    // because old leader was reaped, but defensive).
    pid_numbers.reserve_at(new_tid)
        .ok_or(Errno::EAGAIN)

    // Caller then proceeds with the rename protocol per PROCESS_v1
    // Phase 2 non-leader exec spec, committing the reservation after
    // the PidName/ThreadIdentity binding is updated.
}
```

### 4.3 Raw try_set (case A without RAII)
<!-- txdoc:BITMAP-RESERVATION-USAGE-PATTERNS-RAW-TRY-SET-CASE-A-WITHOUT-RAII-1 -->

```rust
/// Atomic claim of a specific bit, without reservation semantics.
/// Caller is responsible for freeing on error paths manually.
fn raw_claim(bm: &AtomicBitmap<N>, i: u32) -> bool {
    bm.try_set(i)
    // If caller wants to roll back, it must call bm.free(i).
}
```

Preferred only for performance-critical paths where the reservation wrapper's overhead matters. Most callers should use `reserve_at`.

---

## 5. Relationship to the step model
<!-- txdoc:BITMAP-RESERVATION-RELATIONSHIP-TO-THE-STEP-MODEL-1 -->

This substrate extension makes bitmap allocation a first-class participant in the step-model's five-phase discipline (SUBSYSTEM_ANATOMY §3):

| Phase | Role of BitmapReservation |
|---|---|
| 1. observe | — |
| 2. upgrade | — |
| 3. reserve | `reserve()` or `reserve_at(i)` — fallible |
| 4. commit | `commit()` — infallible |
| 5. publish | Bit is set; consumer records the index in the appropriate binding / index entry |

Uncommitted reservations dropping on error in phases 3-4 (before commit) release the bit.

This matches `ZoneReservation::commit` (tx-fnd/zone) and `IndexReservation::commit` (tx-fnd/index) — same pattern, applied to bitmaps.

---

## 6. Testing and invariants
<!-- txdoc:BITMAP-RESERVATION-TESTING-AND-INVARIANTS-1 -->

Properties an implementation must uphold:

- **Mutual exclusion.** At most one `BitmapReservation` exists for a given bit index at a time.
- **Rollback correctness.** Uncommitted reservation's Drop clears the bit; committed reservation's Drop does not.
- **Atomicity of try_set.** Atomic CAS on the word containing the bit; success iff bit was clear.
- **alloc vs reserve fairness.** No starvation: a bit freed via reservation-drop is equally available to `alloc()` and to `reserve_at(i)` as one freed via `free()`.

Tests (in tx-fnd/bitmap):

- Concurrent `reserve()` stress; check each bit allocated at most once.
- `reserve_at` racing: two threads try same bit; exactly one succeeds.
- Reserve + drop (no commit); bit cleared.
- Reserve + commit; bit stays set until explicit `free()`.
- Mixing `alloc`, `reserve`, `try_set` on the same bitmap; counts consistent.

---

## 7. Open questions
<!-- txdoc:BITMAP-RESERVATION-OPEN-QUESTIONS-1 -->

- **Bulk reserve / reserve_range.** Reserving N contiguous bits (useful for some allocators). Not needed by current consumers; can be added if a use case arises.
- **Reserve with hint.** Indicating a preferred region (e.g., for NUMA-local allocation). Out of scope for pid/tid where the namespace is flat.
- **Should commit return the reservation rather than consuming?** No — the reservation's semantic is "claim that may be rolled back"; after commit, the semantic changes to "claim with no rollback." Consuming matches this transition.

---

## 8. Short version
<!-- txdoc:BITMAP-RESERVATION-SHORT-VERSION-1 -->

> Extension of `AtomicBitmap<N>` with three methods: `reserve()` (first free bit with Drop-releases-on-uncommitted), `reserve_at(i)` (specific bit, same rollback), `try_set(i)` (raw atomic set-if-clear). The `BitmapReservation<'_, N>` wrapper provides RAII rollback via Drop; `commit()` consumes self to transfer cleanup responsibility to the caller. Uncommitted rollback is epoch-safe without deferral (no observer holds bit references). Pid-like allocation uses `PidNamespace.numbers` / `AllocIndex`; this primitive remains the appropriate internal allocator for bitmap-backed indexes and for Phase 2 specific-number claims.
