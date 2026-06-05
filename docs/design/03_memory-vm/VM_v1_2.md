# VM Subsystem

<!-- txdoc:03-MEMORY-VM-VM-V1-2 -->

**Status.** v1.2 (2026-04-20). Draft.

**Supersedes.** v1.1. Cross-reference harmonization with CONCEPTS v3 and INVARIANTS v3.3: stale `CONCEPTS §1.3` references updated to `CONCEPTS §1` (third basis claim) and `CONCEPTS §8` (full elaboration); `INVARIANTS_v4.md` reference expanded to list `ARCH-5` explicitly alongside `STEP-4`, `PRED-7`, `SIG-*`; RangeLock positioned as an instance of ARCH-5's slot-locked-with-re-read publication flavor. No architectural changes; text-level sync only.

**Purpose.** Specify the VM subsystem: AddressSpace, VmEntry, the recipes BTree, the RangeLock coordination primitive, and the syscall-level scripts for mmap, munmap, mprotect, mremap, fault handling, fork duplication, exec teardown, and adjacent operations. This document ties together the page substrate ([`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md)) and page-backed model ([`PAGE_BACKED_v1.md`](PAGE_BACKED_v1.md)) into the complete VM story.

**Scope.** Everything from "an AddressSpace exists" to "user-space memory references are handled correctly, including under concurrency." Specifically:

- AddressSpace structure: recipes BTree + pmap + RangeLock.
- VmEntry: the authoritative binding from VA range to backing.
- RangeLock: the structural coordination primitive that admits or excludes concurrent VM operations.
- Per-syscall scripts for VM operations.
- Fault handler for user-space page faults.
- fork's AddressSpace duplication and exec's AddressSpace teardown.
- Race walkthroughs showing invariant preservation under concurrency.

Does *not* cover:

- Frame allocator, FrameMeta, pmap primitives, slab — see [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md).
- PageContainer, RNodeBacking, FsPageBacking, reflink semantics — see [`PAGE_BACKED_v1.md`](PAGE_BACKED_v1.md).
- Reactor, stackless-coroutine executor mechanics — see forthcoming `REACTOR.md`.
- Filesystem operations — see forthcoming `FSOPS.md`.

**Key commitments** (established in prior rounds and assumed throughout):

1. **Authoritative bindings and derived materializations.** Per the publication principle (CONCEPTS §1, third basis claim; elaborated in CONCEPTS §8) and ARCH-5 (INVARIANTS §9), the system partitions state into authoritative bindings (source of truth) and derived materializations (justified by bindings). VM's authoritative binding is the recipes BTree: `(AddressSpace, VA range) → VmEntry`. VM's derived materialization is the pmap: `(AddressSpace, VA) → Frame` via PTE.

2. **No swap.** Anonymous pages are pinned until explicit teardown. The fault handler never needs to page in from swap; it either materializes from a PageContainer (which may block on filesystem I/O for File variant) or zero-fills for anonymous pages.

3. **No KPTI.** Kernel mappings are always present in every AddressSpace's page table root. Fork does not duplicate the kernel high-half; kernel L1 tables are shared by reference.

4. **Per-hart kernel stack, stackless coroutines.** Scripts are Futures; steps are synchronous bounded units returning `StepOutcome<T>`. Blocking for I/O or lock acquisition yields through the reactor.

5. **No rmap.** No reverse mapping from Frame to mapping PTEs. Consequences: certain reflink and migration paths are unavailable; MAP_PRIVATE's private Frames are tracked only via their PTEs, not indexed per-VmEntry.

6. **Address typing is a boundary tool, not the upper-kernel language.** VM owns
   user virtual ranges, fault addresses, pmap materialization addresses, and
   copyin/copyout gates. Above those gates, kernel code speaks semantic
   evidence: `Cap<T>`, `Weak<T>`, `IdentRef<'g, T>`, witnesses, reservations,
   and role tokens. A typed virtual address is not a dereferenceable pointer;
   it becomes a kernel pointer only through HAL/substrate/user-access helpers
   that name the mapping and lifetime.

**Companion documents.**

- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) §1 (third basis claim), §8 (authoritative bindings and derived materializations; justification invariant; publication rule; conditional-commit primitive family).
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — ARCH-5 (justification; publication rule), STEP-4, PRED-7, SIG-*.
- [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md) — frame allocator, FrameMeta, pmap substrate, slab.
- [`PAGE_BACKED_v1.md`](PAGE_BACKED_v1.md) — PageContainer, RNodeBacking, materialize_page.
- [`object_model.md`](../00_meta-framework/object_model_v2.md) §3.3 — Frame compound payload, MapPin.
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — step outcome algebra, retry-on-wake discipline.
- HAL design document — `PmapReservation`, `PmapCommitBatch`, `ShootdownBatch` primitives.

### Zone-derived type policy
<!-- txdoc:VM-ZONE-DERIVED-TYPE-POLICY -->

VM uses policy-based zones only for reclaimable semantic identities. Its
authoritative mapping values and coordination primitives are not promoted to
entities:

| VM declaration | Zone-derived public type | Reclamation role |
|---|---|---|
| `AddressSpace` | `Cap<AddressSpace>`, `Weak<AddressSpace>`, `IdentRef<'g, AddressSpace>` | co-located identity/payload entity retained by process Frame slots |
| `VmEntry` | value stored in the recipes BTree | authoritative binding value, no zone |
| `RangeLock` reservation | linear VM-local reservation token | coordination primitive, no zone |
| PTE / TLB entries | derived materializations | no retention; justified by recipes |
| `Frame` evidence | `MapPin`, `CachePin`, `DmaToken`, or other typed contribution | compound payload counters owned by page substrate |

VM operation code never chooses a raw `Zone<T, Policy>`. It obtains
`AddressSpace` evidence through process Frame slots or witnesses, and it
publishes mapping values through the recipes index.

### Address boundary policy
<!-- txdoc:VM-ADDRESS-BOUNDARY-POLICY -->

VM is the memory subsystem boundary where user address values are meaningful.
It may carry typed `UserVirtAddr`/`UserRange`/`UserPtr<T>`-style values in
syscall arguments, recipes, fault reports, `RangeLock`, and pmap
materialization. These values are still just addresses: they are never
dereferenced directly and never confer authority by themselves.

The authority split is:

- `AddressSpace` identity and lifetime are reached through zone-derived
  evidence (`Cap<AddressSpace>`, `Weak<AddressSpace>`, `IdentRef<'g,
  AddressSpace>`) and process/thread witnesses;
- `VmEntry` values are authoritative mapping bindings in the recipes BTree;
- pmap PTEs are derived materializations justified by recipes and RangeLock;
- user bytes cross the boundary only through the eager-walk
  `AddressSpace::copy_*_user` methods (which walk recipes page-by-page,
  materialise each page through its `VmEntry.backing`, and copy through
  the kernel direct-map view) or through a fault-script materialization
  path that re-reads recipes before PTE publication.

No VM caller should receive a raw kernel pointer to user memory, a freeing
authority encoded as a PPN, or a permission decision encoded only in address
arithmetic. Arithmetic on user addresses is local to VM helpers that align,
split, and index ranges; syscall and subsystem code should consume VM results
as semantic outcomes.

---

## 1. Authoritative bindings and materializations in VM
<!-- txdoc:VM-1-AUTHORITATIVE-BINDINGS-AND-MATERIALIZATIONS-IN-VM -->

Per CONCEPTS §1 (the publication principle, as the third basis claim) and §8 (its elaboration), every materialization in the system exists by virtue of an authoritative binding. Translated to VM:

- **Authoritative binding**: an entry in the recipes BTree mapping a VA range to a VmEntry. This is the truth of "this AddressSpace has a mapping covering this range."
- **Derived materialization**: a PTE in the pmap. A PTE exists because a recipe exists that justifies it. Tearing down the recipe invalidates any PTE it justifies.
- **Further-derived materialization**: TLB entries. These are caches of PTEs, managed by hardware. Shootdown is the invalidation protocol for this further layer of derivation.

The justification invariant for VM:

> For every VA `X` in every AddressSpace `A`, if `A.pmap[X]` contains a PTE, then `A.recipes.range_containing(X)` must return a VmEntry whose permissions permit the PTE's access modes and whose backing resolves to the Frame referenced by the PTE.

This invariant must hold at every observable moment, under arbitrary concurrency. The RangeLock primitive (§3) is the mechanism by which VM enforces it.

**Reads are snapshot-consistent via epoch.** The recipes BTree is persistent; readers observe a consistent snapshot under an epoch guard, concurrent with writers. The pmap is a live hardware structure; per-leaf atomicity comes from HAL's existing primitives.

**Writes are coordinated via RangeLock.** Any operation that mutates recipes or modifies the pmap in ways that could violate the justification invariant acquires a RangeLock reservation over its declared range. This excludes overlapping operations that would otherwise see intermediate states.

### 1.1 Linearization points
<!-- txdoc:VM-1-1-LINEARIZATION-POINTS -->

- **Binding mutations** (`munmap`, `mprotect`, `mremap`, `mmap`'s MAP_FIXED replacement) linearize at the **recipes BTree mutation** (substrate `swap_commit` / `withdraw_commit` / `commit` on the recipes index).
- **Pmap teardown and shootdown** remove **derived materializations** *after* the binding change. Their linearization point is per-PTE (pmap leaf atomicity); collectively, they bring the materialization state into agreement with the new binding state.
- **Materialization publication** (PTE install from the fault handler) linearizes at the **pmap leaf install**, subject to re-verification against current bindings. The RangeLock's Materializer reservation ensures no concurrent ExclusiveWriter is mutating bindings in the target range during publication.

### 1.2 Publication rule
<!-- txdoc:VM-1-2-PUBLICATION-RULE -->

**No materialization may be published unless, at the moment of publication, the operation holds a compatible reservation covering the target range and the authoritative binding matches the observed state used to prepare the publication.**

This rule is enforced by construction:

- The RangeLock's Materializer reservation blocks concurrent ExclusiveWriters in the range; no binding mutation can occur while the reservation is held.
- The fault handler (§5.1) observes the recipe *after* acquiring Materializer and *before* committing the PTE. If any intervening release-reacquire cycle occurred (e.g., due to async I/O), the handler re-observes the recipe on resume and aborts if changed.

---

## 2. AddressSpace structure
<!-- txdoc:VM-2-ADDRESSSPACE-STRUCTURE -->

```rust
pub struct AddressSpace {
    /// Authoritative binding from VA range to VmEntry.
    /// Persistent BTree; snapshot-consistent reads; atomic substrate mutations.
    recipes: PersistentBTree<UserRange, VmEntry>,

    /// Derived materialization: the hardware page table.
    /// Per-leaf atomicity provided by HAL's PmapReservation primitives.
    /// Kernel high-half is shared by reference across all AddressSpaces.
    pmap: Pmap,

    /// Structural coordination primitive: admits or excludes concurrent
    /// VM operations on VA ranges. See §3.
    range_lock: RangeLock,

    /// Cached AddressSpace statistics (rss, vm_size, etc.) for observability.
    /// Not authoritative; derived from recipes and pmap.
    stats: AddressSpaceStats,
}
```

**Stats consistency.** `stats` are derived and **not required to be strongly consistent** with `recipes` or `pmap` at all times. They are updated on commit paths of binding and materialization mutations, but observers may see stats that lag behind the authoritative state by some bounded amount. For observability-grade use (`/proc/<pid>/status`, rlimit enforcement approximations); not suitable for correctness checks.

**Recipes implementation note.** The current implementation realizes
the `PersistentBTree<UserRange, VmEntry>` semantic as an immutable,
structurally shared recipe tree published behind an `AtomicPtr` with
EBR for snapshot-consistent reads. Writers hold the VM-local mutation
lock, path-copy the affected tree nodes, and atomically swap the
published root; readers under an epoch guard see either the pre-mutation
or post-mutation root, never an intermediate rewrite. The key is the
start address of the entry's `UserRange`; range-overlap queries combine
the immediate predecessor with entries whose starts lie inside the
requested range. Adjacent compatible anonymous ranges may coalesce at
commit time, preserving the same authoritative binding while avoiding
recipe growth from page-at-a-time heap extension.
Fork clones the published recipe root directly; child-only CoW metadata
divergence path-copies the affected private entries while unchanged
recipes continue to share nodes.

**Fields are non-negotiable in v1.** Every AddressSpace has exactly these. There is no per-AddressSpace mutex; coordination is through the RangeLock.

**Kernel high-half sharing.** The kernel portion of the page table (the upper 256 L2 slots on Sv39) contains fixed mappings for kernel text/rodata/data, direct map, and MMIO. These L1-level tables are shared by reference: every AddressSpace's root page has its top 256 L2 entries pointing at the same L1 tables. If the kernel ever extends a kernel-high-half mapping post-boot (rare), the update is visible to all AddressSpaces via the shared tables. No per-AddressSpace kernel-range patching needed.

**AddressSpace identity.** AddressSpace is a zone-allocated entity with its own SlotMeta and refcount. Held by processes and threads. Shared across threads of a multithreaded process via `Shared<AddressSpace>` with COW-on-exec semantics (exec replaces the entire AddressSpace; COW fires because the thread group has been reduced to one by exec's prologue).

---

## 3. RangeLock
<!-- txdoc:VM-3-RANGELOCK -->

**RangeLock is a per-AddressSpace structural coordination primitive.** It is part of VM structure in the same sense as recipes and pmap: it does not define semantic mappings itself, but it defines the admissible concurrency on those mappings.

**Relation to ARCH-5.** RangeLock is VM's instance of the **slot-locked with binding re-read** publication flavor (INVARIANTS ARCH-5; CONCEPTS §8.7). The Materializer mode acquires the coordination needed so that a PTE install (materialization) happens under a critical section that encompasses a lock-free snapshot read of the recipes BTree (authoritative binding). The ExclusiveWriter mode handles the range-mutation side: while held, no concurrent Materializer can publish against a stale recipe, and binding withdrawal is sequenced before pmap teardown per ARCH-5's ordering discipline. RangeLock itself is VM-specific; the pattern it instantiates is not.

### 3.1 API
<!-- txdoc:VM-3-1-API -->

```rust
pub struct RangeLock {
    // Internal: concurrent interval structure plus waiter management.
    // Implementation-layer; exact data structure is not part of the contract.
}

pub enum LockMode {
    ExclusiveWriter,
    Materializer,
}

impl RangeLock {
    /// Acquire a reservation on `range` with `mode`.
    ///
    /// Returns Done(guard) on immediate acquisition.
    /// Returns Blocked(WaitToken, HasResponse) if the acquisition would
    ///   conflict with an existing reservation. The caller awaits the
    ///   WaitToken, then re-invokes this step; acquisition is retried
    ///   from scratch.
    /// Never returns Err for "would conflict"; Err is reserved for
    ///   catastrophic internal failure (structure corruption).
    pub fn acquire_step(
        &self,
        range: UserRange,
        mode: LockMode,
    ) -> StepOutcome<RangeGuard>;

    /// Acquire two reservations atomically. Used by mremap (source + dest)
    /// and any future multi-range operation. Acquisition order is internal;
    /// both are acquired or the operation blocks.
    pub fn acquire_pair_step(
        &self,
        a: (UserRange, LockMode),
        b: (UserRange, LockMode),
    ) -> StepOutcome<(RangeGuard, RangeGuard)>;
}
```

**Implementation note.** `RangeLock` exposes both the canonical
spec-shaped surface and a richer dual API for tests and future
writer-preference clients:

- `acquire_step(range, mode) -> StepOutcome<RangeGuard<'_>>` is the
  canonical surface used by all production scripts (`mmap_script`,
  `munmap_script`, `mprotect_script`, `mremap_script`,
  `fault_script`, `brk_script`). It produces `Done(guard)` on
  immediate acquisition and `Blocked(WaitToken)` on contention; async
  callers feed the token to `wait_source::wait_on_token` and retry.
- `acquire_step_rich(range, mode) -> AcquireResult<'_>` is the
  underlying rich variant whose `WouldBlock` carrier holds an internal
  `PendingWriter` slot. The slot pushes back on subsequent
  `Materializer` acquires inside the AVL-backed reservation tree
  (writer-preference). Production callers do not need this; the
  writer-preference unit tests in `vm/tests.rs` consume it via
  `WouldBlock::pending_writer()` and `PendingWriter::try_acquire()`.

`acquire_pair_step` and `acquire_pair_step_rich` follow the same
shape. The canonical method is a thin projection over the rich one
and never produces `Advanced`/`AdvancedThenBlocked`/`Err`.

```rust

#[must_use]
pub struct RangeGuard<'a> {
    lock: &'a RangeLock,
    range: UserRange,
    mode: LockMode,
    // internal handle for O(1) release
}

impl<'a> Drop for RangeGuard<'a> {
    fn drop(&mut self) {
        // Release reservation; wake any waiters whose acquisition
        // may now succeed.
    }
}
```

### 3.2 Lock modes
<!-- txdoc:VM-3-2-LOCK-MODES -->

**ExclusiveWriter.**

Excludes all overlapping reservations. Used for operations that **mutate authoritative bindings** or **invalidate resident materializations** over a range: `mmap`, `munmap`, `mprotect`, `mremap`, fork-parent AS lock, exec teardown.

**Materializer.**

Excludes overlapping `ExclusiveWriter` reservations. Does not exclude overlapping `Materializer` reservations at the range-lock layer.

**Uniqueness and linearization of page publication are not guaranteed by `RangeLock`**; they are enforced by the page-materialization and pmap layers (PageContainer `install_if_absent` at the PC page-index slot; pmap leaf-level atomicity for PTE install). Duplicate publication is resolved below.

Used for operations that publish materializations (PTEs) without mutating recipes: the fault handler, eager prefault during syscall observe phases.

### 3.3 Non-goal
<!-- txdoc:VM-3-3-NON-GOAL -->

**`RangeLock` does not guarantee uniqueness of page publication and does not replace page-level linearization in the PageContainer or pmap.** It only governs **range-level exclusion** between binding mutations and materialization.

- Uniqueness at the PC page-index slot (one Frame per offset) comes from `install_if_absent`.
- Linearization of PTE install comes from pmap leaf-level atomicity.
- RangeLock prevents an ExclusiveWriter from running concurrently with a Materializer in the overlapping range; it does not prevent two Materializers from racing in the same range.

### 3.4 Declared-range reservation rule
<!-- txdoc:VM-3-4-DECLARED-RANGE-RESERVATION-RULE -->

**Declared-range reservation rule (normative).** A `RangeLock` protects the **declared operation range**, not the incidental full extent of any pre-existing `VmEntry` that may be split or replaced while servicing that operation.

This is a first-class architectural commitment. It prevents the design from drifting toward object-shaped locking, where conflict domains would be determined by existing VmEntry boundaries rather than by operation semantics.

Example. Suppose a VmEntry covers `[0x0, 0x10000)`. An mprotect call on `[0x1000, 0x2000)` declares its range as exactly `[0x1000, 0x2000)`. The mprotect acquires an ExclusiveWriter on that declared range. A concurrent fault at `0x5000` — outside the declared range — does **not** conflict, even though the VmEntry that covers `0x5000` is the same VmEntry that mprotect is about to split.

The fault proceeds. It observes the VmEntry as it currently exists (possibly pre-split or post-split), materializes against that VmEntry's backing, installs its PTE. The split of the VmEntry's boundary at `0x1000` and `0x2000` does not alter the content at `0x5000`: same backing, same permissions. The fault's PTE is correct either way.

This rule makes range-lock conflict domains operationally defined: two operations conflict iff their declared ranges overlap. This is the core concurrency property of VM.

### 3.5 Fairness policy
<!-- txdoc:VM-3-5-FAIRNESS-POLICY -->

**Writer-preferred with writers FIFO.**

Once an ExclusiveWriter is queued on a range that overlaps ongoing Materializers, newly arriving overlapping Materializers must also queue behind the writer. Writers are served FIFO among themselves.

**Rationale.** `ExclusiveWriter` corresponds to mutation of authoritative bindings, while `Materializer` corresponds to publication derived from those bindings. Prioritizing writers ensures forward progress of binding state and prevents unbounded delay of mutations under fault-heavy workloads. Under heavy fault pressure (workloads touching fresh anonymous memory rapidly), a pure FIFO policy permits unbounded writer delay because faults can overlap each other and each wake re-starts the queueing race. Writer-preferred makes the semantic priority of authoritative mutations explicit.

This does not starve faults: once a writer completes and releases, faults unblock in FIFO order among themselves. Starvation of faults is bounded by the number of queued writers.

### 3.6 Cross-async-wait discipline
<!-- txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE -->

**A reservation protects only the synchronous publication or rewrite phase of an operation. It must not be held across unbounded asynchronous waits.**

If a step yields while preparing publication (e.g., the fault handler blocking on `materialize_page` for a disk read), it **must drop the reservation** before yielding. Upon resume, it **must reacquire** the reservation and **must re-observe all authoritative state** from the beginning of the step.

Rationale. Holding a RangeLock reservation across disk I/O would serialize an entire range against every concurrent operation for the duration of the I/O — potentially tens of milliseconds. The reservation is for synchronous coordination of binding-and-materialization consistency, not for blocking other threads while waiting on hardware.

Application. The fault handler (§5.1) acquires a Materializer reservation, re-observes recipes, calls `materialize_page`. If `materialize_page` returns `Blocked`, the handler drops the reservation and its guards, yields, and on wake retries from the top. The second acquisition re-runs `acquire_step` (cheap), re-observes recipes (possibly changed during the wait), and continues.

### 3.7 WaitToken abstraction
<!-- txdoc:VM-3-7-WAITTOKEN-ABSTRACTION -->

A blocked acquisition returns a `WaitToken` that causes the caller to be rescheduled when reservation state may have changed in a way that could admit retry.

**A `WaitToken` does not guarantee that acquisition will succeed upon wake**; it only indicates that reservation state **may have changed**. Callers **must retry acquisition from the beginning of the step**. A wake is a hint, not a grant.

The `WaitToken` is a bus carrier (per `SIG-1`: wake is not truth; fresh observation authorizes action).

**Implementation note.** The mapping from release events to waiter wakeups is **implementation-defined**. v1 may implement `WaitToken` using a single RawPort per `RangeLock`, fired on every reservation release (thundering herd). This is acceptable given the expected contention profile (low number of concurrent VM operations per AddressSpace). Finer-grained wake mechanisms — per-waiter channels or per-held-reservation wake lists — are implementation optimizations that may be adopted if profiling shows wakeup churn matters.

### 3.8 Guard lifecycle
<!-- txdoc:VM-3-8-GUARD-LIFECYCLE -->

RangeGuard is RAII-bound to a stack frame. Its lifetime `'a` ensures it cannot outlive the RangeLock. Drop fires release unconditionally (success or failure of the protected operation). Operations that fail partway (e.g., fault hitting a permission error after acquiring Materializer) drop the guard via normal control flow; no leak discipline needed.

---

## 4. VmEntry
<!-- txdoc:VM-4-VMENTRY -->

The authoritative binding value in the recipes BTree.

```rust
pub struct VmEntry {
    /// VA range. Exact bounds of the mapping. Always page-aligned.
    range: UserRange,

    /// Permissions: birth protection bits. Current effective permissions
    /// for access control. Hardware PTEs installed for this VmEntry must
    /// not exceed these permissions.
    prot: Prot,

    /// Mapping flags. MAP_SHARED vs MAP_PRIVATE, MAP_GROWSDOWN (stack),
    /// MAP_FIXED (for creation), MAP_LOCKED (noop under no-swap but 
    /// recorded for observability), etc.
    flags: VmEntryFlags,

    /// What backs this mapping.
    backing: VmBacking,
}

pub enum VmBacking {
    /// File-backed, shm-backed, tmpfs-backed, device-backed, or 
    /// anonymous-with-SHARED mappings. A PageContainer provides the 
    /// pages; offset identifies where within the PC this VmEntry starts.
    Page {
        pc: Cap<PageContainer>,
        offset: u64,
    },

    /// MAP_ANONYMOUS | MAP_PRIVATE with no shared backing.
    /// Reads return zero (via a shared read-only zero Frame).
    /// Writes allocate private Frames that are tracked only via their
    /// PTEs, not indexed in any PC.
    PrivateAnon,

    /// MAP_NONE: address space reservation with no mapping.
    /// Used for PROT_NONE guard pages and for some mmap patterns
    /// that reserve address space without committing.
    None,
}

pub struct Prot {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

pub struct VmEntryFlags {
    pub shared: bool,      // MAP_SHARED vs MAP_PRIVATE
    pub grows_down: bool,  // MAP_GROWSDOWN; stack-like growth
    pub locked: bool,      // MAP_LOCKED; observational under no-swap
    // more as needed
}
```

**Storage.** VmEntries are stored as values in the recipes BTree. They are cheap to clone (a few fields plus a `Cap<PageContainer>` which is a refcount bump). Splits during partial munmap/mprotect produce new VmEntries with narrower ranges, same other fields.

**Immutability in the recipes BTree.** A VmEntry inside the recipes BTree is immutable. Modifications (prot change, range shrink/grow) are realized by replacing the VmEntry via `swap_commit` under an ExclusiveWriter reservation. This keeps epoch-protected readers safe: an IdentRef observed before the swap sees the old VmEntry fully, not a half-modified one.

### 4.1 Special case: shared zero Frame
<!-- txdoc:VM-4-1-SPECIAL-CASE-SHARED-ZERO-FRAME -->

For `VmBacking::PrivateAnon` reads before any write, there's a global read-only zero Frame that the fault handler installs:

```rust
static ZERO_FRAME: Frame = ...;  // initialized at boot; reserved = true
```

The ZERO_FRAME is a normal Frame whose FrameMeta has `flags.direct_mapped = 1` and `flags.reserved = 1`. It's never freed. Every PTE pointing at it for read-only access increments its map_count normally.

**Overflow semantics.** FrameMeta's `map_count` is 10 bits (ceiling 1023). ZERO_FRAME is the only Frame expected to approach this ceiling in practice — a system with 1000+ processes each reading untouched anonymous pages accumulates ZERO_FRAME PTEs. **If ZERO_FRAME's `map_count` would overflow, the offending PTE install fails with `ENOMEM`.** The faulting thread receives SIGBUS on that access. This condition is expected to be rare; in the common case, the 1023 ceiling is ample because most processes write to their anonymous memory early, converting read-only ZERO_FRAME references to private-Frame references.

When a write fault hits a PTE pointing at ZERO_FRAME, the fault handler recognizes the case: allocate a fresh private Frame, zero it (already zero from alloc_frame_zeroed), install the fresh Frame in the PTE, decrement ZERO_FRAME's map_count.

---

## 5. Syscall scripts
<!-- txdoc:VM-5-SYSCALL-SCRIPTS -->

Each script is an `async fn` that executes one or more steps. Each step follows STEP-4's five phases. RangeLock reservations scope to the synchronous mutation phase; I/O waits drop the reservation.

### 5.1 Fault handler
<!-- txdoc:VM-5-1-FAULT-HANDLER -->

```rust
async fn fault_script(
    ctx: &ThreadContext,
    va: VAddr,
    access: AccessMode,
) -> Result<(), SigInfo> {
    let page_range = UserRange::containing_page(va);

    loop {
        // Phase: acquire Materializer reservation on the faulted page.
        let guard = match ctx.aspace.range_lock.acquire_step(
            page_range,
            LockMode::Materializer,
        ) {
            Done(g) => g,
            Blocked(token, m) => {
                wait_on(token, m).await?;
                continue;
            }
            Err(_) => return Err(SigInfo::sigsegv(va)),
        };

        // Phase: observe recipes under epoch guard (within reservation).
        let epoch_guard = epoch::pin();
        let recipe_ref = match ctx.aspace.recipes.range_containing(va, &epoch_guard) {
            Some(r) => r,
            None => return Err(SigInfo::sigsegv(va)),
        };

        // Phase: validate permissions for the access.
        if !recipe_ref.prot.permits(access) {
            return Err(SigInfo::sigsegv_with_reason(va, SegvReason::PermissionMismatch));
        }

        // Phase: materialize the page.
        let frame = match resolve_frame(&recipe_ref, va, &epoch_guard) {
            Done(f) => f,
            Blocked(token, m) => {
                drop(guard);           // drop reservation per §3.6
                drop(epoch_guard);
                wait_on(token, m).await?;
                continue;              // restart from acquire
            }
            Err(e) => return Err(SigInfo::for_fault_error(e)),
        };

        // Phase: install PTE. Under Materializer reservation, no ExclusiveWriter
        // on this range can interfere. Pmap atomicity via HAL's PmapReservation
        // at leaf granularity.
        match pmap_install_pte(
            &ctx.aspace.pmap,
            va,
            &frame,
            effective_perms(&recipe_ref, access),
        ) {
            Ok(()) => return Ok(()),
            Err(PmapInstallError::AlreadyPresent) => {
                // Raced with another Materializer on same page; their PTE is
                // now there, equivalent to ours. Our frame reference drops;
                // if we allocated it, its cache_ref handles cleanup.
                return Ok(());
            }
            Err(e) => return Err(SigInfo::for_pmap_error(e)),
        }
    }  // guard drops here on success path via normal return
}
```

`resolve_frame` is the helper that dispatches by `VmBacking`:

```rust
fn resolve_frame<'g>(
    recipe: &VmEntry,
    va: VAddr,
    guard: &'g Guard,
) -> StepOutcome<IdentRef<'g, Frame>> {
    match &recipe.backing {
        VmBacking::Page { pc, offset } => {
            let pc = pc.upgrade(guard)?;
            let content_offset = *offset + (va.0 - recipe.range.start.0) as u64;
            materialize_page(&pc, content_offset, guard)
        }
        VmBacking::PrivateAnon => {
            // Read access: install the shared zero Frame.
            // Write access: allocate a fresh private Frame (see fault_script,
            // which handles the write-vs-read distinction before calling this).
            if is_read_access() {
                Done(ZERO_FRAME.as_ident_ref(guard))
            } else {
                let ppn = alloc_frame_zeroed().ok_or(Errno::ENOMEM)?;
                Done(Frame::from_ppn(ppn).as_ident_ref(guard))
            }
        }
        VmBacking::None => Err(Errno::EFAULT),
    }
}
```

### 5.2 mmap
<!-- txdoc:VM-5-2-MMAP -->

```rust
async fn mmap_script(
    ctx: &ThreadContext,
    addr: Option<VAddr>,
    len: usize,
    prot: Prot,
    flags: MapFlags,
    fd: Option<Fd>,
    offset: u64,
) -> Result<VAddr, Errno> {
    // Phase 1: resolve target range.
    let target_range = resolve_target_range(
        &ctx.aspace,
        addr, len, flags,
    )?;

    // Phase 2: resolve backing.
    let backing = resolve_mmap_backing(ctx, fd, offset, len, flags)?;

    // Phase 3: acquire ExclusiveWriter on target range.
    let guard = loop {
        match ctx.aspace.range_lock.acquire_step(
            target_range,
            LockMode::ExclusiveWriter,
        ) {
            Done(g) => break g,
            Blocked(t, m) => wait_on(t, m).await?,
            Err(_) => return Err(Errno::ENOMEM),
        }
    };

    // Phase 4: install recipe.
    // For MAP_FIXED, existing recipes in range are replaced (via substrate swap).
    // For non-fixed, range was chosen free by phase 1, so install is via commit.
    let vm_entry = VmEntry {
        range: target_range,
        prot,
        flags: flags.into(),
        backing,
    };

    match flags.is_fixed() {
        true => {
            // MAP_FIXED: remove any existing recipes in range, then install.
            // Also tear down any overlapping PTEs before releasing reservation.
            withdraw_recipes_in_range(&ctx.aspace.recipes, target_range);
            let mut shootdown = ShootdownBatch::new();
            teardown_ptes_in_range(&ctx.aspace.pmap, target_range, &mut shootdown);
            shootdown.issue_and_wait();
            install_recipe(&ctx.aspace.recipes, vm_entry);
        }
        false => {
            install_recipe(&ctx.aspace.recipes, vm_entry);
        }
    }

    Ok(target_range.start)
    // guard drops here
}
```

The `resolve_target_range` helper finds free address space for non-fixed mmap. It consults recipes under an epoch guard to find a gap big enough; if no gap is available, returns ENOMEM. The gap is not reserved by this step — another concurrent mmap could grab it before phase 3 acquires the reservation. If so, phase 3 blocks (or resolves a fresh range on retry). This is fine under the declared-range rule: the range is chosen fresh each attempt.

Eager prefault is not performed by default. Pages materialize on first access via the fault handler. Callers passing MAP_POPULATE may trigger a prefault loop after mmap returns; this is a separate policy decision, implementable as a loop of `read_access_at(va)` calls after mmap completes.

### 5.3 munmap
<!-- txdoc:VM-5-3-MUNMAP -->

```rust
async fn munmap_script(
    ctx: &ThreadContext,
    addr: VAddr,
    len: usize,
) -> Result<(), Errno> {
    let range = UserRange::new_aligned(addr, len)?;

    let guard = loop {
        match ctx.aspace.range_lock.acquire_step(
            range,
            LockMode::ExclusiveWriter,
        ) {
            Done(g) => break g,
            Blocked(t, m) => wait_on(t, m).await?,
            Err(_) => return Err(Errno::EINVAL),
        }
    };

    // Withdraw recipes in range. Partial-overlap VmEntries split.
    withdraw_recipes_in_range(&ctx.aspace.recipes, range);

    // Tear down PTEs and shoot down TLB entries.
    let mut shootdown = ShootdownBatch::new();
    teardown_ptes_in_range(&ctx.aspace.pmap, range, &mut shootdown);
    shootdown.issue_and_wait();

    // Deferred map_count decrements happen inside ShootdownBatch's completion.

    Ok(())
    // guard drops
}
```

### 5.4 mprotect
<!-- txdoc:VM-5-4-MPROTECT -->

**v1 policy:** `mprotect` rewrites recipes and invalidates overlapping PTEs. It **does not perform in-place permission retagging** of existing PTEs.

This policy is explicit, not accidental. It means:

- Recipes are rewritten under ExclusiveWriter: existing VmEntries in the range are replaced with new VmEntries carrying the new prot.
- All PTEs in the range are torn down and shotdown.
- Subsequent faults re-materialize with the new prot.

The alternative — walk the pmap and patch PTEs' permission bits in place — is semantically equivalent in the common case but reopens concurrency questions (what if the patch races with a fault installing a PTE?). The teardown-and-refault approach is cleaner.

```rust
async fn mprotect_script(
    ctx: &ThreadContext,
    addr: VAddr,
    len: usize,
    new_prot: Prot,
) -> Result<(), Errno> {
    let range = UserRange::new_aligned(addr, len)?;

    let guard = loop {
        match ctx.aspace.range_lock.acquire_step(
            range,
            LockMode::ExclusiveWriter,
        ) {
            Done(g) => break g,
            Blocked(t, m) => wait_on(t, m).await?,
            Err(_) => return Err(Errno::EINVAL),
        }
    };

    // Validate that new_prot is compatible with each VmEntry's birth prot
    // (cannot raise above original permissions — POSIX constraint).
    if !can_prot_range(&ctx.aspace.recipes, range, new_prot) {
        return Err(Errno::EACCES);
    }

    // Rewrite recipes: replace VmEntries in range with new-prot versions.
    // Partial-overlap VmEntries split into in-range (new prot) and out-of-range
    // (old prot) pieces.
    rewrite_recipes_prot_in_range(&ctx.aspace.recipes, range, new_prot);

    // Tear down all PTEs in range. They will re-materialize with new prot.
    let mut shootdown = ShootdownBatch::new();
    teardown_ptes_in_range(&ctx.aspace.pmap, range, &mut shootdown);
    shootdown.issue_and_wait();

    Ok(())
    // guard drops
}
```

### 5.5 mremap
<!-- txdoc:VM-5-5-MREMAP -->

Relocates a mapping from source to destination.

**v1 policy: if `old_range` and `new_range` overlap, the operation fails with `EINVAL`.** Canonicalizing to a union range is representable but significantly complicates the pair-acquisition and the recipe rewrite (it would become a single-range in-place rewrite with content shift). The simpler disjoint-ranges-only rule suffices for v1; the overlapping case is rare in practice (programs that want to expand-in-place use `mremap(..., old_addr, new_len, 0)` without passing a new_addr, which the kernel either satisfies in place by extending the VmEntry or rejects with EAGAIN).

```rust
async fn mremap_script(
    ctx: &ThreadContext,
    old_addr: VAddr,
    old_len: usize,
    new_addr: Option<VAddr>,
    new_len: usize,
    flags: MremapFlags,
) -> Result<VAddr, Errno> {
    let old_range = UserRange::new_aligned(old_addr, old_len)?;
    let new_range = resolve_new_range(&ctx.aspace, new_addr, new_len, flags)?;

    // v1: reject overlap.
    if old_range.overlaps(new_range) {
        return Err(Errno::EINVAL);
    }

    // Acquire both reservations atomically.
    let (g_old, g_new) = loop {
        match ctx.aspace.range_lock.acquire_pair_step(
            (old_range, LockMode::ExclusiveWriter),
            (new_range, LockMode::ExclusiveWriter),
        ) {
            Done(pair) => break pair,
            Blocked(t, m) => wait_on(t, m).await?,
            Err(_) => return Err(Errno::ENOMEM),
        }
    };

    // Validate: new_range is either empty or the caller specified MREMAP_FIXED
    // with permission to overwrite.
    if !is_dest_valid(&ctx.aspace.recipes, new_range, flags) {
        return Err(Errno::EFAULT);
    }

    // Read the source VmEntry(s) from recipes. Compute destination VmEntry(s).
    let src_entries = enumerate_recipes_in_range(&ctx.aspace.recipes, old_range);
    let dst_entries = rebase_entries(&src_entries, old_range, new_range);

    // Withdraw source recipes; install destination recipes.
    withdraw_recipes_in_range(&ctx.aspace.recipes, old_range);
    for entry in dst_entries {
        install_recipe(&ctx.aspace.recipes, entry);
    }

    // Pmap: tear down source PTEs; shootdown. Destination PTEs materialize
    // on fault.
    let mut shootdown = ShootdownBatch::new();
    teardown_ptes_in_range(&ctx.aspace.pmap, old_range, &mut shootdown);
    shootdown.issue_and_wait();

    Ok(new_range.start)
    // guards drop
}
```

An alternative implementation moves PTEs directly (source PTE at `X` is reinstalled at `X - old.start + new.start`). This avoids refaulting but requires careful pmap mechanics for cross-range PTE moves. v1 chooses the simpler teardown+refault; optimization is future work.

### 5.6 fork
<!-- txdoc:VM-5-6-FORK -->

Fork duplicates the parent's AddressSpace into the child. The child doesn't exist as a target of VM operations yet (not yet scheduled), so contention is only with other parent threads.

```rust
async fn fork_aspace(
    parent: &AddressSpace,
) -> Result<AddressSpace, Errno> {
    // Acquire ExclusiveWriter on full parent AS.
    let guard = loop {
        match parent.range_lock.acquire_step(
            UserRange::full_user_v1(),
            LockMode::ExclusiveWriter,
        ) {
            Done(g) => break g,
            Blocked(t, m) => wait_on(t, m).await?,
            Err(_) => return Err(Errno::ENOMEM),
        }
    };

    // Clone the recipes BTree. Persistent structure: O(1) root clone.
    let child_recipes = parent.recipes.clone();

    // Construct child pmap by walking parent's pmap.
    // For MAP_SHARED entries: PTEs copy directly, map_count increments.
    // For MAP_PRIVATE entries: PTEs copy with write bit cleared in both
    //   parent and child, for CoW on next write.
    let child_pmap = walk_and_duplicate_pmap(&parent.pmap, &child_recipes)?;

    // Shootdown parent's MAP_PRIVATE PTEs that were demoted to read-only.
    // (Shootdown covers only the demoted ranges, not all of parent.)
    let mut shootdown = ShootdownBatch::new();
    for range in map_private_ranges_in(&child_recipes) {
        queue_demoted_ptes_for_shootdown(&parent.pmap, range, &mut shootdown);
    }
    shootdown.issue_and_wait();

    Ok(AddressSpace::new_with(
        child_recipes,
        child_pmap,
        RangeLock::new(),
        AddressSpaceStats::new(),
    ))
    // guard drops
}
```

**v1 policy: fork serializes all parent VM operations for its duration.** This design **intentionally serializes** all concurrent VM activity in the parent during fork (v1 simplification). Breaking fork into range-by-range pmap walks, allowing concurrent VM ops on disjoint ranges, is a potential v2 optimization.

### 5.7 exec
<!-- txdoc:VM-5-7-EXEC -->

Exec replaces the entire AddressSpace. Before exec is called, the process's other threads have been killed (exec's prologue). No concurrent VM operations exist.

```rust
async fn exec_aspace(
    old_aspace: &Arc<AddressSpace>,
    new_image: &ExecImage,
) -> Result<AddressSpace, Errno> {
    // No RangeLock needed — single-threaded by construction.
    // But we acquire one for pattern uniformity, which is cheap since
    // uncontended.
    let guard = match old_aspace.range_lock.acquire_step(
        UserRange::full_user_v1(),
        LockMode::ExclusiveWriter,
    ) {
        Done(g) => g,
        _ => unreachable!("exec is single-threaded"),
    };

    // Drop everything in the old AS. Recipes BTree dropped (Drop cascades
    // to VmEntries; their PC Caps drop). Pmap torn down entirely.
    drop_recipes(&old_aspace.recipes);
    teardown_whole_pmap(&old_aspace.pmap);

    // Construct new AS from ExecImage.
    let new_aspace = construct_aspace_from_image(new_image)?;

    // No shootdown needed: this thread will satp-switch to the new pmap,
    // and no other thread of this process exists. Self-invalidation via
    // local sfence.vma is sufficient.

    Ok(new_aspace)
    // guard drops; old AS reclaimed
}
```

### 5.8 brk
<!-- txdoc:VM-5-8-BRK -->

Legacy heap growth. Implemented as automatic mmap of anonymous region with extension semantics.

```rust
async fn brk_script(
    ctx: &ThreadContext,
    new_brk: VAddr,
) -> Result<VAddr, Errno> {
    let old_brk = ctx.aspace.brk_current.load(Ordering::Acquire);
    
    if new_brk > old_brk {
        // Extend heap: mmap anonymous region from old_brk to new_brk.
        mmap_script(
            ctx,
            Some(old_brk),
            (new_brk - old_brk).as_usize(),
            Prot { read: true, write: true, execute: false },
            MapFlags::anonymous_private(),
            None,
            0,
        ).await?;
    } else if new_brk < old_brk {
        // Shrink heap: munmap region from new_brk to old_brk.
        munmap_script(
            ctx,
            new_brk,
            (old_brk - new_brk).as_usize(),
        ).await?;
    }
    
    ctx.aspace.brk_current.store(new_brk, Ordering::Release);
    Ok(new_brk)
}
```

### 5.9 madvise, msync, mincore
<!-- txdoc:VM-5-9-MADVISE-MSYNC-MINCORE -->

These are mostly thin wrappers around existing primitives.

**madvise** under no-swap: MADV_DONTNEED is tear-down-and-shootdown (range-scoped, like mini-munmap but preserving the VmEntry). MADV_WILLNEED is a prefault hint (optional; v1 is no-op). MADV_FREE is equivalent to MADV_DONTNEED for anonymous memory. Most others are no-ops.

**msync** with MS_SYNC forces writeback of dirty File-variant PC pages in the range. Uses PageContainer's step_fsync machinery. Acquires ExclusiveWriter because writeback may race with concurrent writes (and we want to capture a consistent snapshot).

**mincore** is a pure read operation under epoch: walk the pmap in the range, report per-page presence as a `Vec<bool>` whose `i`-th entry is `true` iff the page at offset `i` of the requested range has a published pmap entry at the moment it is read. **v1 policy: `mincore` does not acquire a reservation.** It observes pmap state under best-effort consistency: the result reflects some interleaving of concurrent VM operations but is not a strongly-consistent snapshot. POSIX permits this. Callers who need tight consistency should serialize externally with the operations they care about. The `Vec<bool>` shape matches POSIX `mincore(2)`'s per-page residency vector; per-syscall scripts copy it into the user buffer.

---

## 6. Race walkthroughs
<!-- txdoc:VM-6-RACE-WALKTHROUGHS -->

Each race closes via the RangeLock reservation.

### 6.1 Multi-fault on same VA
<!-- txdoc:VM-6-1-MULTI-FAULT-ON-SAME-VA -->

Two threads fault on VA X concurrently.

- Both acquire Materializer on `[X, X+PAGE)`. Materializer does not exclude Materializer; both proceed concurrently.
- Both observe recipes, both call `materialize_page`.
- `materialize_page` uses PageContainer's page-index `install_if_absent`: one thread's Frame wins; the other's Frame (if it allocated one) drops via the page index's rejection.
- Both threads have the same Frame reference now.
- Both try to install PTE at VA X via pmap. HAL's PmapReservation serializes per-leaf. One installer succeeds; the other gets `AlreadyPresent` and treats it as success (the PTE is already there, pointing at the same Frame).

Result: one Frame materialized in PC, one PTE in pmap, both faults complete successfully.

### 6.2 Fault vs munmap on same range
<!-- txdoc:VM-6-2-FAULT-VS-MUNMAP-ON-SAME-RANGE -->

Fault at VA X; munmap on `[A, B)` where X ∈ [A, B).

**Sub-case 6.2a: Fault acquires first.**
- Fault holds Materializer on `[X, X+PAGE)`.
- munmap's ExclusiveWriter on `[A, B)` requests — overlaps Fault's Materializer → blocks on WaitToken.
- Fault completes: observes recipes (VmEntry present), materializes, installs PTE, releases reservation.
- munmap wakes: acquires ExclusiveWriter. Withdraws recipes in [A, B). Tears down PTEs in [A, B) — including the PTE fault just installed. Shootdown. map_count decrement.
- User thread that faulted: its user-space access succeeds if completed before munmap's shootdown reaches its hart; otherwise re-faults and SIGSEGVs (recipe gone).

**Sub-case 6.2b: munmap acquires first.**
- munmap holds ExclusiveWriter on `[A, B)`.
- Fault's Materializer on `[X, X+PAGE)` requests — overlaps → blocks.
- munmap: withdraws recipes, tears down PTEs, shootdown, releases.
- Fault wakes: acquires Materializer. Observes recipes: VmEntry absent at X. Returns SIGSEGV.

Both outcomes are POSIX-conformant for "access during concurrent munmap." No silent UAF in either case.

### 6.3 Fault vs mprotect on same range
<!-- txdoc:VM-6-3-FAULT-VS-MPROTECT-ON-SAME-RANGE -->

Fault at VA X; mprotect on `[A, B)` making prot stricter.

**Sub-case 6.3a: Fault acquires first.**
- Fault holds Materializer, observes old prot, materializes with old prot, releases.
- mprotect acquires ExclusiveWriter, rewrites recipes, tears down all PTEs in range (including fault's), shootdown, releases.
- User thread sees new prot on next access (because PTEs torn down; refault observes new recipes).

**Sub-case 6.3b: mprotect acquires first.**
- mprotect updates recipes and tears down old PTEs.
- Fault acquires Materializer after mprotect releases. Observes new recipes: new prot. Validates access against new prot. If access is compatible: materialize. If access is now forbidden (e.g., write to PROT_READ range): SIGSEGV.

### 6.4 Fault vs mremap
<!-- txdoc:VM-6-4-FAULT-VS-MREMAP -->

Fault at VA X in old_range; mremap moving old_range to new_range.

- mremap acquires ExclusiveWriter on both old and new range via `acquire_pair_step`.
- Fault's Materializer on X (which is in old_range) blocks on mremap's reservation.
- mremap: withdraw recipes from old_range, install recipes at new_range, teardown old PTEs, shootdown, release.
- Fault wakes: observes recipes. `recipes.range_containing(X)` returns None (X's VmEntry moved to a different range).
- Fault returns SIGSEGV. User thread sees the memory at X as unmapped, because it is.

If the user wanted to access the mremap'd content, they must reference the new address (mremap's return value).

### 6.5 Concurrent mprotect on overlapping ranges
<!-- txdoc:VM-6-5-CONCURRENT-MPROTECT-ON-OVERLAPPING-RANGES -->

Two mprotect calls: A on `[0, 1000)` and B on `[500, 1500)`.

Both want ExclusiveWriter on their range. Overlap is `[500, 1000)`. RangeLock serializes: whichever arrived first (by FIFO among writers) acquires; the other waits.

After first completes and releases, second wakes, acquires, proceeds. Its recipe rewrite operates on whatever state the first left behind. No race.

### 6.6 Concurrent mprotect on disjoint ranges
<!-- txdoc:VM-6-6-CONCURRENT-MPROTECT-ON-DISJOINT-RANGES -->

A on `[0, 1000)`; B on `[2000, 3000)`. No overlap → no conflict. Both acquire concurrently, both proceed, both release. Writer concurrency for disjoint ranges is preserved.

### 6.7 Fault vs fork
<!-- txdoc:VM-6-7-FAULT-VS-FORK -->

Parent thread faults at VA X; parent thread initiates fork.

Fork acquires ExclusiveWriter on full user range. Fault's Materializer on `[X, X+PAGE)` blocks (overlap).

Fork completes: recipes cloned, pmap walk done, shootdown for demoted PTEs issued, release.

Fault wakes: observes (possibly modified) recipes. Its PTE install on parent's pmap is now against a pmap that's had some PTEs demoted to read-only (the MAP_PRIVATE regions). If the fault's access was a write to a MAP_PRIVATE region that got demoted, the fault handler recognizes this (it's a write access against a read-only PTE that was just installed by fork's demotion), CoWs appropriately.

### 6.8 Exec during nothing
<!-- txdoc:VM-6-8-EXEC-DURING-NOTHING -->

Exec runs in a process where exec's prologue has killed all sibling threads. No concurrent VM ops exist. Exec takes ExclusiveWriter uncontested, tears down, rebuilds.

---

## 7. MAP_PRIVATE CoW details
<!-- txdoc:VM-7-MAP-PRIVATE-COW-DETAILS -->

MAP_PRIVATE mappings require copy-on-write semantics. Writes don't propagate to the PC; a write fault allocates a private Frame.

### 7.1 Read fault on MAP_PRIVATE
<!-- txdoc:VM-7-1-READ-FAULT-ON-MAP-PRIVATE -->

- Observe recipe: MAP_PRIVATE VmEntry with `VmBacking::Page { pc, offset }` or `VmBacking::PrivateAnon`.
- For Page: materialize the page from PC (shared read). Install PTE as read-only (even though VmEntry has write prot; the read-only bit triggers CoW on write).
- For PrivateAnon: install PTE pointing at ZERO_FRAME, read-only.

### 7.2 Write fault on MAP_PRIVATE
<!-- txdoc:VM-7-2-WRITE-FAULT-ON-MAP-PRIVATE -->

- Observe recipe. Permission check: write must be in prot.
- Observe existing PTE (if any): read-only.
- Allocate fresh private Frame. Copy content from current source (either the PC's Frame or ZERO_FRAME).
- Install PTE pointing at private Frame, writable.
- If the PTE previously pointed at a shared Frame (from PC or ZERO_FRAME), its map_count decrements after shootdown of the old PTE.
- Private Frame is tracked only via the PTE. Its cache_ref is 0; only map_count and refcount hold it.

### 7.3 Private Frames have no back-index
<!-- txdoc:VM-7-3-PRIVATE-FRAMES-HAVE-NO-BACK-INDEX -->

A private Frame lives in pmap only. It's not in any PC page index. Consequences:

- Reclaim under memory pressure cannot evict private Frames (no way to find them except by walking all pmaps). Under no-swap, this is fine — anonymous pages can't be reclaimed anyway.
- Process exit's pmap teardown decrements all private Frames' map_counts, triggering their free.
- No operation walks "all private Frames for this VmEntry"; none is needed.

---

## 8. VmEntry splitting and merging
<!-- txdoc:VM-8-VMENTRY-SPLITTING-AND-MERGING -->

Range operations often split existing VmEntries. The substrate mutation primitives on the recipes BTree handle this atomically.

**Split on partial overlap.** mprotect on `[0x1000, 0x2000)` where an existing VmEntry covers `[0x0, 0x10000)`: the VmEntry splits into three — `[0x0, 0x1000)` with old prot, `[0x1000, 0x2000)` with new prot, `[0x2000, 0x10000)` with old prot. Atomically via BTree swap.

**Merge on adjacent compatible VmEntries.** mmap adjacent to an existing VmEntry with identical backing, prot, flags may merge them into a single larger entry. Optimization; not required for correctness. Skipped in v1.

**Split/merge is BTree-level atomic.** Under ExclusiveWriter reservation, the substrate primitive `recipes::rewrite_range(range, new_entries_list)` performs the split atomically. Readers under epoch see either pre-rewrite or post-rewrite, never intermediate.

---

## 9. Tech debt and deferred items
<!-- txdoc:VM-9-TECH-DEBT-AND-DEFERRED-ITEMS -->

### 9.1 No rmap
<!-- txdoc:VM-9-1-NO-RMAP -->

**Consequence.** Cannot efficiently answer "what PTEs reference this Frame across all AddressSpaces?" This blocks:

- Reflink with writable-shared concurrent mappings (handled by EBUSY return; see PAGE_BACKED §7 tech debt).
- Page migration (NUMA balancing, memory hot-remove).
- Swap-out (not needed under no-swap commitment).

**Eventual fix.** Add rmap if and when we have a concrete need. The overhead is per-Frame back-pointer management; Linux pays it ubiquitously.

### 9.2 No hugetlb, no transparent huge pages
<!-- txdoc:VM-9-2-NO-HUGETLB-NO-TRANSPARENT-HUGE-PAGES -->

**Consequence.** All mappings are 4 KiB granularity. Workloads that would benefit from 2 MB pages (databases, JVMs) pay full TLB cost.

**Eventual fix.** hugetlbfs-style explicit huge pages first; THP is post-v1.

### 9.3 userfaultfd — Phase 1 landed
<!-- txdoc:VM-9-3-NO-USERFAULTFD -->

**Status.** Phase 1 (PR-10) implemented. `UFFDIO_REGISTER` tags VMAs with a `UfdRegistration` key; the fault path dispatches to a userfaultfd agent via `UfdDispatch`, yielding `OnAgent` for page-backed and private-anon fault targets. `UFFDIO_COPY` byte-move materialises agent-provided pages under the Materializer lock. `NullUfdDispatch` is the default no-op dispatcher for the non-UFD path.

**Deferred.** `UFFDIO_ZEROPAGE`, `UFFDIO_WAKE`, `UFFDIO_WRITEPROTECT`, `UFFDIO_CONTINUE`, and non-PageBacked/PrivateAnon backing support are not yet implemented. Range-registration partial-VMA splitting is out of scope for phase 3.

### 9.4 mlock as observation only
<!-- txdoc:VM-9-4-MLOCK-AS-OBSERVATION-ONLY -->

Under no-swap, all pages are effectively pinned anyway (nothing can evict them). `mlock` succeeds, sets a VmEntry flag for observability (`/proc/<pid>/maps` shows locked regions), but takes no kernel action beyond that.

### 9.5 Fork serializes all parent VM operations
<!-- txdoc:VM-9-5-FORK-SERIALIZES-ALL-PARENT-VM-OPERATIONS -->

**Consequence.** Highly multithreaded processes pay latency on fork. Linux has similar behavior via `mmap_lock` write mode.

**Eventual fix.** Break fork into range-by-range pmap walks, allowing concurrent VM ops on disjoint ranges. Adds complexity; deferred.

### 9.6 No MAP_HUGETLB, MAP_STACK, MAP_UNINITIALIZED
<!-- txdoc:VM-9-6-NO-MAP-HUGETLB-MAP-STACK-MAP-UNINITIALIZED -->

**Consequence.** A few mmap flag-variants not supported in v1.

**Eventual fix.** Add as encountered.

### 9.7 MADV_WILLNEED is a no-op
<!-- txdoc:VM-9-7-MADV-WILLNEED-IS-A-NO-OP -->

Prefaulting hints from userspace are not honored in v1.

### 9.8 In-place PTE permission patching not attempted
<!-- txdoc:VM-9-8-IN-PLACE-PTE-PERMISSION-PATCHING-NOT-ATTEMPTED -->

**Consequence.** mprotect teardowns and refaults all PTEs in range, even when the permission change is a relaxation that could be patched in place (e.g., adding PROT_WRITE to a PROT_READ region would just need to flip the write bit, not tear down the PTE). This is a minor perf cost.

**Eventual fix.** Implement in-place PTE patching with careful concurrency analysis (possibly under ExclusiveWriter serialization, which we already have). Not urgent.

### 9.9 Fairness may need tuning under pathological workloads
<!-- txdoc:VM-9-9-FAIRNESS-MAY-NEED-TUNING-UNDER-PATHOLOGICAL-WORKLOADS -->

Writer-preferred policy may starve faults in adversarial scenarios. No evidence of this in realistic workloads. If it becomes a problem, tune the policy or introduce bounded writer priority boosts.

### 9.10 Single WaitToken per RangeLock (v1)
<!-- txdoc:VM-9-10-SINGLE-WAITTOKEN-PER-RANGELOCK-V1 -->

Thundering-herd on release. Acceptable for expected contention profiles. Upgrade to finer-grained wake model if profiling shows wakeup churn.

### 9.11 RangeLock is VM-specific
<!-- txdoc:VM-9-11-RANGELOCK-IS-VM-SPECIFIC -->

Not promoted to substrate. Other subsystems don't currently need range-based coordination. If a future subsystem does, the RangeLock pattern can be generalized — but the implementation should live with its primary consumer, not abstracted prematurely.

### 9.12 User-page gifts for `vmsplice`
<!-- txdoc:VM-9-12-USER-PAGE-GIFTS-FOR-VMSPLICE -->

`vmsplice(SPLICE_F_GIFT)` needs page-granular transfer without changing VM's
authoritative granularity. The VM contract is therefore a materialized transfer
token:

```rust
pub struct UserPageGift { /* VM-private fields */ }
pub struct GiftBatch { /* ordered full-page gifts plus copied tails */ }

pub enum UserPageGiftFreeze {
    DetachedPrivate,
    DemotedCow,
}
```

`UserPageGift` is not a `VmEntry`, not a page object, and not a page-level
authoritative binding. It is linear evidence that a VM operation observed an
eligible user page, materialized it to a frame, acquired substrate transfer
retention for that frame, and revoked or demoted the old writable user
materialization before publishing the token to a pipe descriptor.

The VM primitive is:

```rust
pub fn gift_user_pages_step(
    aspace: Cap<AddressSpace>,
    iov: UserRange,
    flags: GiftFlags,
) -> StepOutcome<GiftBatch>;
```

The step obligations are:

1. **Observe.** Validate the user range and split it into page-aligned units.
   Unaligned heads/tails are reported to the caller for byte-copy fallback.
2. **Acquire.** Take `RangeLock::Materializer` over each declared page range
   that may be gifted. The reservation is still range-scoped; it does not
   introduce a page lock or page-shaped VM binding.
3. **Re-observe recipe.** Re-read the recipes BTree under the reservation. v1
   eligibility is full-page aligned private anonymous or private CoW material
   only. `MAP_SHARED`, device mappings, missing mappings, and unsupported
   page-backed states are rejected for gift and handled by copy fallback.
4. **Materialize.** Resolve the page to a concrete frame through the normal
   fault/materialization path. If the step blocks, drop the reservation and
   retry from observe on wake.
5. **Reserve transfer evidence.** Acquire substrate `GiftPin` evidence for the
   live frame. In the first implementation slice this is retained-frame
   transfer evidence on `FrameMeta.refcount`, not DMA `pin_count`.
6. **Freeze user ownership.** Remove or demote writable PTE materialization
   before the token is visible outside VM. Private anonymous pages become
   `DetachedPrivate`; private CoW sources become `DemotedCow`. Releasing the
   token never restores the old writable PTE; a future user write refaults and
   takes the normal CoW path.
7. **Publish.** Return `UserPageGift` values to the syscall script. Pipe stores
   them only as ordered descriptors. PageBacked is the consumer that installs
   or copies the gifted frame into a destination `PageContainer`.

This preserves the existing VM rule: recipes remain authoritative range
bindings and PTEs remain derived materializations. Page gifting changes only the
operation that prepares a transfer token; it does not make pages independently
owned VM resources.

---

## 10. Summary
<!-- txdoc:VM-10-SUMMARY -->

VM's architecture:

- **AddressSpace** = recipes BTree (authoritative) + pmap (materialization) + RangeLock (coordination).
- **VmEntry** is the authoritative binding value; `VmBacking` has three variants (Page-backed, PrivateAnon, None).
- **RangeLock** admits or excludes VM operations by range and mode. ExclusiveWriter excludes all overlap; Materializer excludes only ExclusiveWriter.
- **Declared-range rule** fixes conflict domains to operation-declared ranges, not to pre-existing VmEntry extents.
- **Writer-preferred, writers-FIFO fairness** prioritizes binding mutations over materializations.
- **Reservations drop across async waits**; resume re-acquires and re-observes.
- **Syscall scripts** acquire reservations, perform recipe mutations and pmap changes, release. Each script is a small `async fn` with well-defined step boundaries.
- **Fault handler** is a Materializer-taking script; other operations (mmap, munmap, mprotect, mremap, fork-parent, exec) are ExclusiveWriter-taking scripts.
- **Races resolve by reservation** — no silent UAF, no corrupted materializations, SIGSEGV on operations against withdrawn bindings.

Approximately 1200 lines of spec. The architecture is substantially cleaner than Linux's VM (which has mmap_sem, anon_vma, shadow objects, rmap, and layered locks) because of the upstream commitments: no swap, no KPTI, no rmap, authoritative-binding/derived-materialization discipline with RangeLock as the coordination primitive.

---

## References
<!-- txdoc:VM-REFERENCES -->

- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) §1 (third basis claim: publication principle), §8 (authoritative bindings, derived materializations, justification invariant, publication rule, conditional-commit primitive family).
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — ARCH-5 (justification; publication rule), STEP-4 (five-phase discipline), PRED-7 (anti-TOCTOU), SIG-* (publication discipline).
- [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md) — frame allocator, FrameMeta, pmap substrate, slab.
- [`PAGE_BACKED_v1.md`](PAGE_BACKED_v1.md) — PageContainer, RNodeBacking, materialize_page, reflink.
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — step outcome algebra, retry-on-wake.
- [`object_model.md`](../00_meta-framework/object_model_v2.md) §3.3, §5, §6, §7 — compound payload, reference hierarchy, reclamation.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §4 — substrate primitives (zone, index, credit, mutation); §4.5 is the realization catalog for ARCH-5's publication rule.
- HAL design document — `PmapReservation`, `PmapCommitBatch`, `ShootdownBatch`.