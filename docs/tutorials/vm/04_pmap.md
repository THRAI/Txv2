# Chapter 4 — Pmap: the derived materialization

The recipes (Chapter 3) say what *should* be mapped. The pmap is what the
hardware is *actually* using: the page-table entries the MMU walks, plus a
software shadow of them. This chapter is the VM side of that — how PTEs are
published from recipes and torn back down. Chapter 5 goes below it, into the
`PmapIf` HAL trait and the real Sv39 page tables.

## A materialization, not a truth

Hold onto the framing from Chapter 2: a PTE is a *derived materialization*. It
exists only because a recipe justifies it; it carries no truth of its own; it can
be discarded and rebuilt at any page. The `VmPmap` is the manager of these
derived things — it never *decides* a mapping, it only *realizes* (or unrealizes)
decisions the recipes already made.

```rust
// vm/pmap.rs:157
pub struct VmPmap {
    root: Option<PmapRoot>,   // the HAL page-table root for this address space
    ops: VmPmapOps,           // function pointers into the platform PmapIf
    state: Mutex<VmPmapState>,// the resident shadow store + counters
}
```

`root` is opaque HAL evidence — a handle to the hardware page-table root,
obtained at construction via `new_for_platform::<P: PmapIf>()` (`pmap.rs:164`),
which also stitches in the shared kernel high-half (Chapter 5). `ops` is the set
of platform primitives. `state` holds the **resident store** — the software
mirror of which pages are currently materialized — and statistics.

## What a published mapping looks like

```rust
// vm/pmap.rs:85
pub struct PmapMapping {
    pub ppn: Ppn,                  // the physical frame this page maps to
    pub prot: Prot,                // the permissions actually installed
    pin: MaterializedPagePin,      // the MapPin (or device pin) holding the frame live
}
```

That `pin` field is the bridge to Chapter 2's Frame disjunction. A live PTE owns
a `MaterializedPagePin` — a `MapPin` on the frame — which is one disjunct of
`Frame.payload_live`. Install a PTE, acquire a `MapPin`, `map_count` goes up; tear
the PTE down, drop the pin, `map_count` goes down. The frame stays alive as long
as *some* address space maps it *or* the page cache holds it. The pmap holds
exactly the mapped disjunct, one pin per PTE.

For read-only observation there is a cap-free, pin-free view:

```rust
// vm/pmap.rs:109
pub struct PmapMappingSnapshot { pub ppn: Ppn, pub prot: Prot }
```

Returned by `lookup(page)` (`pmap.rs:185`) and `walk_range(range)` (`pmap.rs:200`)
— the latter is what `mincore` uses to report residency (Chapter 10). A snapshot
leaks no pin: looking at the materialization does not keep it alive.

## Publishing a PTE: reserve, then commit

Installing a mapping is a two-phase HAL handshake — reserve (which may allocate
intermediate page-table nodes), then commit (which writes the leaf):

```rust
// vm/pmap.rs:237  (simplified)
pub fn publish_page_with_replacement(
    &self, page: UserPage, ppn: Ppn, prot: Prot,
    map_pin: MaterializedPagePin, replace_existing: bool,
) -> Result<PmapPublishOutcome, VmPmapError> {
    let mut state = self.state.lock();

    if let Some(existing) = state.mappings.get(&page) {
        if existing.ppn == ppn && existing.prot == prot {
            return Ok(PmapPublishOutcome { page, replaced: false }); // idempotent
        }
        if !replace_existing {
            return Err(VmPmapError::MappingMismatch);   // someone else's PTE; refuse
        }
        // replace: unmap the old leaf, shoot it down, drop its pin
        ...
    }

    let reservation = (self.ops.reserve_mapping)(root, virt, phys, PmapReserveKind::Page4K)?;
    (self.ops.commit_mapping)(root, reservation, permissions_for_prot(prot));
    state.mappings.insert(page, PmapMapping::new(ppn, prot, map_pin));   // track it
    Ok(PmapPublishOutcome { page, replaced })
}
```

Three things to notice:

- **Idempotent republish is free.** If the exact same `(ppn, prot)` is already
  there, return immediately. This is what makes two threads faulting the same
  page safe: the loser of the race finds the winner's identical PTE and treats it
  as success (Chapter 7).
- **A *different* mapping at the same page is refused** (`MappingMismatch`)
  unless the caller explicitly asked to replace. The fault path never replaces;
  the operations that legitimately overwrite a page (CoW break, `UFFDIO_COPY`)
  pass `replace_existing = true`.
- **The shadow store is updated in lockstep** with the hardware. `state.mappings`
  always reflects what the HAL was last told. `publish_page` (`pmap.rs:227`) is
  the thin wrapper that calls this with `replace_existing = false`.

The reserve/commit split exists because reserving may need to allocate L1/L0
page-table nodes (which can fail with `MissingReservation`) before any leaf is
written — so a failure leaves the hardware untouched and rolls back cleanly. The
mechanics of that allocation are Chapter 5.

`publish_new_pages_best_effort` (`pmap.rs:308`) is the batch cousin used for
speculative prefault (clustered anonymous write faults): it publishes a run of
pages and silently drops the pins of any that fail, because prefault is an
optimization, never an obligation.

## The resident store: a shadow, not a truth

`state.mappings` is the **resident store** — a software index of every page this
address space currently has materialized. It is emphatically *not* authoritative
(the recipes are); it exists so the VM can answer "what is mapped here right now"
without walking hardware page tables, which is needed for teardown, `mprotect`,
`fork`, `exec`, and drop.

It comes in two backends (`vm/pmap/resident.rs`):

- **`VecPmapResidentStore`** (default, `resident.rs:142`) — an address-sorted
  `Vec` with binary-search lookup. Simple and compact. Its weakness: draining a
  range shifts every entry after it, so a big `munmap` or process exit is O(n) in
  the suffix.
- **`ChunkedPmapResidentStore`** (cfg `tx_vm_pmap_chunked_resident`,
  `resident.rs:236`) — 64-entry chunks, so a range drain removes whole chunks
  without a global suffix shift. It trades a little per-insert cost for amortized
  zero-shift teardown.

`drain_range` (`resident.rs:68`) reports a `shifted_entries` count — the number
of entries that had to move — which is exactly the metric the chunked backend
exists to drive to zero. This is a pure performance knob; both backends present
the same interface and the same semantics.

## Tearing down: withdraw recipe first, then PTEs

`teardown_range(range)` (`pmap.rs:380`) removes every PTE in a range, drops their
`MapPin`s (so `map_count` falls), and batches the TLB invalidations for a single
shootdown. Critically, **it is called *after* the recipe has already been
withdrawn or rewritten** — never before. That ordering is the justification
invariant in action (Chapter 1): the binding leads, the materialization follows.
A concurrent fault in the same range is excluded by the `RangeLock` writer the
caller holds (Chapter 6), so there is no window where a fault could re-publish a
PTE against a binding that is already gone.

`protect_range(range, prot)` (`pmap.rs:468`) is the materialization side of
`mprotect`. Recall from Chapter 2 that `mprotect` is a *binding rewrite* — so the
common path is "rewrite the recipe, then `teardown_range` so the next fault
re-materializes with the new permissions." `protect_range` exists for the cases
that demote in place without a full teardown (notably `fork`'s read-only demotion,
Chapter 8), flipping the write bit on existing leaves and shooting them down.

## Issuing the shootdown

Teardown collects its invalidations and issues them in a batch. On a uniprocessor
this is a local `sfence.vma`; on SMP it becomes cross-hart IPIs scoped to the
address space's ASID. `PmapStats` (`pmap.rs:115`) counts `mapped_pages`,
`reservations`, `commits`, `rollbacks`, and `shootdowns` for observability. The
actual cross-hart mechanism — `sbi_remote_sfence_vma_asid` gated by an
ASID-residency bitmap — is the subject of the next chapter; here the pmap's job
ends at "hand the batch to the HAL."

## Where we go from here

You have the VM side of materialization: PTEs published from recipes through a
reserve/commit handshake, mirrored in a resident shadow store, torn down after
the binding changes, each PTE holding one `MapPin` disjunct of the frame's
liveness. Chapter 5 drops below the `ops` function pointers into the `PmapIf` HAL
trait: the Sv39 three-level walk, page-table-node allocation, the shared kernel
high-half that makes `fork` cheap and KPTI unnecessary, the `satp` switch, and
the SMP TLB shootdown.

## Source anchors

- `VmPmap` struct: `crates/tx-subsystems/src/vm/pmap.rs:157`
- Construction (`new_for_platform`): same file, `:164`
- `PmapMapping` (incl. the `MaterializedPagePin`): same file, `:85`
- `PmapMappingSnapshot` / `lookup` / `walk_range`: same file, `:109, 185, 200`
- `publish_page` / `publish_page_with_replacement`: same file, `:227, 237`
- Batch prefault (`publish_new_pages_best_effort`): same file, `:308`
- `teardown_range` / `protect_range`: same file, `:380, 468`
- `PmapStats` / `VmPmapError` / `PmapPublishOutcome`: same file, `:115, 124, 145`
- Resident store backends: `crates/tx-subsystems/src/vm/pmap/resident.rs:142` (Vec), `:236` (chunked); `drain_range` `:68`
- `MapPin` / Frame disjunction: `docs/design/00_meta-framework/object_model_v2.md:131`; `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md` §3.1
