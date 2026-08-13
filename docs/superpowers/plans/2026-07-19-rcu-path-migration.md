# RCU Path Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current lock-backed Zone key resolution and fixed-pool EBR retirement path with lock-free constant-time observation, three intrusive per-CPU epoch bags, and `Published<RecipeTree>` while preserving owner-facing APIs.

**Architecture:** The migration keeps three paths separate. Zone entities retain compact `Cap<T>`/`Weak<T>` identity and gain a fixed-depth lock-free slab directory; generic immutable roots use private `RcuHead` nodes through `Published<T>`; VM remains the first owner-level pilot and keeps private `Arc` tree nodes. The epoch engine provides one shared grace-period mechanism, but allocation/reclaim policy stays private to Zone, publication, and VM.

**Tech Stack:** Rust `no_std`, atomic memory ordering, per-CPU state, tx-hal platform traits, tx-substrate Zone/epoch/publication, tx-subsystems VM, `cargo xtask` ratchets.

---

## 1. Required Path Contract

The migration is not complete until these bounds hold in the default build:

| Path | Required bound | Lock rule |
|---|---:|---|
| `epoch::guard` / Guard Drop | `O(1)` | no spinlock |
| `SlotKey -> Slot<T>` | fixed-depth `O(1)` | no Keg lock and no slab-list walk |
| `Weak::observe` / `Cap::deref` | `O(1)` plus metadata validation | no allocation/list lock |
| local retire enqueue | `O(1)` | CPU-local exclusion only; no callback, allocation, CPU scan, or drain |
| local eligible detach | `O(budget)` | CPU-local exclusion; callbacks outside exclusion |
| epoch advance | `O(online CPUs)` | membership lock only for admission/offline, not the scan |
| published-root read | one Acquire root load | no writer lock |
| Recipe point lookup | `O(tree height)` | no writer lock |
| Recipe overlap walk | `O(tree height + output)` | no writer lock |

Allowed linear work remains explicit:

- epoch advance scans participating CPUs;
- snapshot and overlap operations may visit their output;
- physical destruction is proportional to released storage, but it runs after
  epoch exclusion and is scheduled through bounded maintenance batches;
- CPU offline may scan CPUs and transfer all bags owned by the offlining CPU.

The following are non-goals for this migration:

- moving RecipeTree nodes into Zone storage;
- exposing `Cap<Node>`, `Weak<Node>`, `RcuHead`, bag phases, or raw pointers;
- replacing the RecipeTree backend or forcing the B+ backend on;
- migrating PageContainer, SocketTable, fd tables, mount tables, or namespace
  maps before the Recipe pilot closes;
- removing semantic writer serialization from RecipeIndex.

## 2. File Ownership Map

| Area | Files | Responsibility |
|---|---|---|
| HAL exclusion and IPI | `crates/tx-hal/src/lib.rs`, active board `lib.rs`/platform files, `crates/tx-kernel/src/trap.rs`, `crates/tx-kernel/src/init.rs` | real local IRQ/preemption exclusion and maintenance kick transport |
| Epoch core | `crates/tx-substrate/src/epoch/{domain,guard,local,bag,mod}.rs` | guard ordering, per-CPU bags, advance, drain, membership/offline |
| Zone resolution | `crates/tx-substrate/src/zone/{directory,keg,registry,slab,cap}.rs` | fixed-depth slab lookup; Keg lock remains allocation/reclaim-only |
| Zone lifecycle | `crates/tx-substrate/src/zone/{meta,slot,reservation,cap,registry}.rs` | four-state metadata, intrusive SlotKey link, quarantine, typed reclaim |
| Publication | `crates/tx-substrate/src/publication/{mod,node}.rs`, `crates/tx-substrate/src/lib.rs` | `Published<T>` and private generic intrusive nodes |
| Raw-retire cleanup | `crates/tx-substrate/src/bus/{owner,macros,mod}.rs` | remove unused public raw owner-storage retirement vocabulary |
| VM pilot | `crates/tx-subsystems/src/vm/structure/{recipe,address_space,recipe_tree}.rs` | replace manual `AtomicPtr`/`retire_raw`, preserve owner API |
| Ratchets | `xtask/src/lint_invariants_{zone,api_language}.rs`, `xtask/src/lint.rs` | prevent raw retirement, raw publication, and backend type leakage |
| Tests | `crates/tx-substrate/tests/{epoch,zone,ap_init,bus}.rs`, VM unit tests | ordering, complexity counters, rollback, SMP/offline, pilot equivalence |

## Task 1: Freeze Complexity And Visibility Ratchets

**Files:**
- Modify: `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`
- Modify: `docs/design/00_meta-framework/OBJECT_API_LANES_v1.md`
- Modify: `xtask/src/lint_invariants_zone.rs`
- Modify: `xtask/src/lint_invariants_api_language.rs`
- Modify: `xtask/Cargo.toml`
- Test: `xtask/src/lint_invariants_zone.rs`

- [x] **Step 1: Add the fixed-depth Zone lookup contract**

Add a `txdoc:`-tagged paragraph requiring `SlotKey` resolution to use a stable,
fixed-depth directory. State that Keg lists and their lock are allocation and
reclaim metadata only and are forbidden from `Weak::observe`, `Cap::deref`,
clone, and non-final Drop.

- [x] **Step 2: Add path-complexity assertions to the active contract**

Record the table in Section 1 of this plan in
`EBR_ZONE_INTERFACE_v1.md`. Keep `O(CPU)` advance and output-sensitive tree
walks explicit so the ratchet does not prohibit necessary work.

- [x] **Step 3: Add linter tests before changing ceilings**

Add unit cases equivalent to:

```rust
#[test]
fn rejects_raw_retire_outside_backend_allowlist() {
    assert_violation(
        "crates/tx-subsystems/src/vm/structure/recipe.rs",
        "epoch::retire_raw(ptr, reclaim)",
        "raw-retire",
    );
}

#[test]
fn rejects_published_type_in_owner_facade_signature() {
    assert_violation(
        "crates/tx-subsystems/src/vm/mod.rs",
        "pub fn recipes(&self) -> &Published<RecipeTree>",
        "publication-backend-leak",
    );
}
```

Start the raw-retire ceiling at the mechanically measured production count and
decrease it at Tasks 5, 7, and 8. Never raise the ceiling.

- [x] **Step 4: Verify the contract-only slice**

Run:

```bash
cargo xtask lint docs
cargo test -p xtask lint_invariants_zone --lib
cargo test -p xtask 'lint_invariants_api_language::tests::rcu_ratchet_' --lib
cargo xtask lint invariants api-language
cargo xtask progress validate
```

Expected: docs lint and focused linter tests pass; no runtime Rust path changes.

- [ ] **Step 5: Commit the ratchet contract**

Deferred in the shared dirty checkout: this execution was not authorized to
stage or commit. Task 1's implementation and gates are complete independently
of repository publication.

```bash
git add docs/design/00_meta-framework/OBJECT_API_LANES_v1.md \
  docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md \
  xtask/src/lint_invariants_zone.rs xtask/src/lint_invariants_api_language.rs
git commit -m "docs: freeze rcu path complexity contract"
```

## Task 2: Make SlotKey Resolution Lock-Free And Fixed-Depth

**Files:**
- Create: `crates/tx-substrate/src/zone/directory.rs`
- Modify: `crates/tx-substrate/src/zone/mod.rs`
- Modify: `crates/tx-substrate/src/zone/keg.rs`
- Modify: `crates/tx-substrate/src/zone/reservation.rs`
- Modify: `crates/tx-substrate/src/zone/mod.rs`
- Modify: `crates/tx-substrate/src/epoch/{bag,domain,local}.rs`
- Modify: `crates/tx-substrate/src/zone/slab.rs`
- Modify: `crates/tx-substrate/src/zone/registry.rs`
- Test: `crates/tx-substrate/tests/zone.rs`

- [x] **Step 1: Write collision and lock-acquisition regressions**

Create enough `LargeObject` allocations to exceed 64 slabs and collide in the
old direct-mapped cache. Under a Guard, observe Weak handles from the first,
middle, and last slab. Assert all resolve and that the test-only Keg lock counter
does not change during observation, dereference, clone, or non-final Drop.

- [x] **Step 2: Verify the regressions fail on the current Keg cache**

Run:

```bash
cargo test -p tx-substrate --test zone zone_lookup_is_lock_free_across_cache_collisions
```

Expected: FAIL because `Keg::slot_from_key` acquires `self.lock`.

- [x] **Step 3: Add a two-level slab directory**

Use the 18 slab-id bits implied by the 24-bit slot id and 64 slots per slab.
Use 9 high bits and 9 low bits. The root and leaf pages are lazily allocated on
the Keg allocation path and remain stable for the static Zone lifetime.

```rust
const DIRECTORY_BITS: usize = 9;
const DIRECTORY_WIDTH: usize = 1 << DIRECTORY_BITS;

pub(crate) struct SlabDirectory<T: 'static> {
    root: AtomicPtr<SlabDirectoryRoot<T>>,
}

struct SlabDirectoryRoot<T: 'static> {
    leaves: [AtomicPtr<SlabDirectoryLeaf<T>>; DIRECTORY_WIDTH],
}

struct SlabDirectoryLeaf<T: 'static> {
    slabs: [AtomicPtr<ZoneSlab<T>>; DIRECTORY_WIDTH],
}
```

Reader lookup performs one root Acquire load, one leaf Acquire load, one slab
Acquire load, slab-id validation, and direct slot pointer arithmetic. It never
touches a slab list or Keg lock.

- [x] **Step 4: Publish and unpublish directory entries on the writer path**

Publish the slab pointer with Release ordering before any SlotKey can escape.
Clear the entry with Release ordering before unlinking/retiring an empty slab.
Slab storage remains EBR-delayed, so a Guard that observed the old pointer may
finish safely.

- [x] **Step 5: Remove the 64-entry `slab_cache` read path**

Delete `SLAB_CACHE_SIZE`, `cache_store_locked`, `cache_clear_locked`, and the
list-walking fallback from `slot_from_key`. Keep the three Keg lists solely for
free-space classification and slab lifecycle.

- [x] **Step 6: Verify Zone behavior and fixed-depth lookup**

Run:

```bash
cargo test -p tx-substrate --test zone
cargo test -p tx-substrate --test ap_init
cargo -q xtask unit
```

Expected: all tests pass; the collision test reports zero reader-path Keg lock
acquisitions.

- [ ] **Step 7: Commit the reader-path correction**

Deferred in the shared dirty checkout: this execution was not authorized to
stage or commit. The scoped implementation and verification are complete;
`cargo -q xtask unit` remains blocked before the touched crate by unrelated
dirty Reactor API drift.

```bash
git add crates/tx-substrate/src/zone crates/tx-substrate/tests/zone.rs
git commit -m "refactor: make zone slot resolution lock free"
```

## Task 3: Provide Real CPU-Local Exclusion

**Files:**
- Modify: `crates/tx-hal/src/lib.rs`
- Modify: `boards/tx-hal-riscv64-qemu-virt/src/lib.rs`
- Modify: `boards/tx-hal-loongarch64-qemu-virt/src/platform_impls.rs`
- Modify: `boards/tx-hal-riscv64-m1dock-mock/src/lib.rs`
- Modify: host test platform implementations in `crates/tx-substrate/tests/epoch.rs`
- Test: `crates/tx-substrate/tests/epoch.rs`

- [x] **Step 1: Write nesting and restoration tests**

Test that local exclusion disables interrupt admission, preserves the previous
interrupt-enabled state, restores it exactly once on Drop, and is `!Send` and
`!Sync`. Add a same-CPU simulated IRQ retire attempt and assert it cannot enter
while exclusion is held.

- [x] **Step 2: Add one HAL-owned RAII primitive**

Keep the public foundation vocabulary to one type:

```rust
pub struct LocalExecutionGuard {
    restore: unsafe fn(usize),
    saved_state: usize,
    _not_send_sync: PhantomData<*mut ()>,
}

pub trait IrqIf {
    fn exclude_local_execution() -> LocalExecutionGuard;
}
```

The active boards implement architecture-specific interrupt save/disable and
restore. `CpuPinGuard` continues to carry CPU affinity; the local critical
section must not yield, allocate, or invoke callbacks.

- [x] **Step 3: Add crate-private `LocalRetireGuard`**

`LocalRetireGuard` contains `CpuPinGuard` and `LocalExecutionGuard`, validates
CPU admission, sets `retire_active` with Release ordering, and clears it before
restoring local execution. Do not add a spinlock fallback.

- [x] **Step 4: Verify HAL and epoch tests**

Run:

```bash
cargo test -p tx-substrate --test epoch local_retire_guard_
cargo check -p tx-hal
cargo check -p tx-hal-riscv64-qemu-virt
cargo check -p tx-hal-loongarch64-qemu-virt
```

Expected: tests pass and no local-retire test acquires a spinlock.

- [ ] **Step 5: Commit the exclusion substrate**

Deferred in the shared dirty checkout: this execution was not authorized to
stage or commit. Scoped epoch and cross-architecture HAL verification passed;
broader gates remain blocked by unrelated Reactor and step-agent test API drift.

```bash
git add crates/tx-hal boards crates/tx-substrate/src/epoch \
  crates/tx-substrate/tests/epoch.rs
git commit -m "feat: add cpu local retirement exclusion"
```

## Task 4: Add Three Intrusive Per-CPU Bags Beside Compatibility Retirement

**Files:**
- Create: `crates/tx-substrate/src/epoch/bag.rs`
- Modify: `crates/tx-substrate/src/epoch/local.rs`
- Modify: `crates/tx-substrate/src/epoch/domain.rs`
- Modify: `crates/tx-substrate/src/epoch/mod.rs`
- Test: `crates/tx-substrate/tests/epoch.rs`

- [x] **Step 1: Write bounded-detach and reentrancy tests**

Add tests that enqueue more nodes than the budget, record both `examined` and
`reclaimed`, and assert `examined <= budget + constant_bag_count`. Add a callback
that retires another node and prove it runs after local exclusion is released.

- [x] **Step 2: Define private bag storage**

```rust
#[repr(C)]
pub(crate) struct RcuHead {
    pub(crate) next: *mut RcuHead,
    pub(crate) reclaim: unsafe fn(*mut RcuHead),
}

pub(crate) struct EpochBag {
    epoch: u64,
    zone_head: Option<SlotKey>,
    rcu_head: *mut RcuHead,
}

pub(crate) struct LocalRetireState {
    bags: [EpochBag; 3],
}
```

Store `UnsafeCell<LocalRetireState>` inside each `CpuLocalEpochState`. Do not
construct `&mut DomainState` from one domain-wide `UnsafeCell`.

Move `active_guards` accounting out of the global reader-written cacheline.
Correctness reads `local_epoch`; summaries may aggregate per-CPU counters or
use debug-only accounting.

- [x] **Step 3: Implement no-allocation enqueue**

The generic enqueue method accepts only a held `LocalRetireGuard`. It samples
the global epoch with the documented AcqRel RMW, selects `epoch % 3`, validates
the bag tag, and pushes one intrusive head in `O(1)`. Task 4 reserves the Zone
head; Task 5 wires the Zone enqueue method after `Retiring(next)` exists in slot
metadata, so the engine does not invent a second temporary Zone link format.

- [x] **Step 4: Implement bounded detach and callback-outside-exclusion**

Detach at most `budget` total nodes from reclaimable heads and preserve
remainder ownership in each bag. Clear `retire_active` and release local IRQ
exclusion before callbacks while retaining the no-yield CPU-affinity witness;
re-establish exclusion before touching per-CPU state again. Empty, open,
waiting, and reclaimable remain derived conditions, not enums.

- [x] **Step 5: Keep current `retire_raw` only as an explicit compatibility lane**

During Tasks 4-7, the fixed pool remains solely for the mechanically counted
raw callsites. New Zone and publication code is forbidden from using it. Add
stats separating `compat_retired` from intrusive bag counts.

- [x] **Step 6: Verify bag ordering and bounds**

Run:

```bash
cargo test -p tx-substrate --test epoch intrusive_bag_
cargo test -p tx-substrate --test epoch drain_callback_can_retire_reentrantly
cargo test -p tx-substrate --test epoch drain_examines_only_budgeted_nodes
```

Expected: all tests pass; retire enqueue never allocates or calls drain.

- [ ] **Step 7: Commit the internal bag engine**

Deferred in the shared dirty checkout: this execution was not authorized to
stage or commit. Task 4's implementation, review, and scoped gates are complete
independently of repository publication.

```bash
git add crates/tx-substrate/src/epoch crates/tx-substrate/tests/epoch.rs
git commit -m "feat: add intrusive per cpu epoch bags"
```

## Task 5: Migrate Zone Lifecycle And Slab Retirement

**Files:**
- Modify: `crates/tx-substrate/src/zone/meta.rs`
- Modify: `crates/tx-substrate/src/zone/cap.rs`
- Modify: `crates/tx-substrate/src/zone/slot.rs`
- Modify: `crates/tx-substrate/src/zone/registry.rs`
- Modify: `crates/tx-substrate/src/zone/slab.rs`
- Modify: `crates/tx-substrate/src/zone/keg.rs`
- Test: `crates/tx-substrate/tests/zone.rs`

- [x] **Step 1: Write lifecycle and generation exhaustion tests**

Cover the exact sequence `Free -> Reserved -> Live -> Retiring -> Free`, a
clone racing final Drop, a Weak observation racing retirement, raw SlotKey zero
as a non-null list member, and generation `u16::MAX` quarantine with no slot
reuse.

Deterministic host coverage includes the four-state grace sequence, guarded
Weak observation, raw SlotKey zero, mixed-zone dispatch, compatibility-pool
exhaustion, and generation quarantine. True clone/upgrade and stale-directory
reader races are part of Task 6's SMP witness.

- [x] **Step 2: Contract SlotWord to four states**

Remove only `SlotState::Dead`; keep the existing typed `Dead` operation error.
Use spare metadata bits for `has_next` and `generation_exhausted`. In
`Retiring`, reinterpret the 32 retain bits as the next `SlotKey`.

- [x] **Step 3: Make final Cap Drop one infallible compound transition**

Under `LocalRetireGuard`, CAS `Live(retain=1)` to `Retiring(next=none)`, sample
the post-barrier epoch, write the old Zone head into metadata, and publish the
slot as the new bag head. If retain CAS loses to clone/upgrade, retry; after the
CAS wins there is no error or synchronous drain path.

- [x] **Step 4: Reclaim Zone slots by SlotKey dispatch**

Add a crate-private typed reclaim callback to the registry entry. Reclaim loads
the intrusive next key before destroying `T`, clears the link, increments or
quarantines generation, publishes Free with Release ordering, then returns a
non-quarantined slot to the Keg.

- [x] **Step 5: Embed `RcuHead` in whole-slab storage**

Whole slabs are generic allocations, not Zone slots. Add a private `RcuHead`
to `ZoneSlab<T>` and migrate Keg empty-slab retirement to generic intrusive
enqueue. Keep slab unpublication before enqueue and frame release after grace.

- [x] **Step 6: Verify Zone and bus regressions**

Run:

```bash
cargo test -p tx-substrate --test zone
cargo test -p tx-substrate --test bus
cargo -q xtask unit
```

Expected: Zone has no `RetiredNodePoolExhausted` path; `Weak` and `Cap` behavior
is unchanged at the public boundary.

- [ ] **Step 7: Lower the raw-retire ratchet and commit**

Zone production raw-retire callsites are zero; the ratcheted direct-call
baseline remains one because RecipeIndex is the next pilot. Commit is deferred
because staging/commit was not authorized in the shared dirty checkout.

```bash
git add crates/tx-substrate/src/zone crates/tx-substrate/tests/zone.rs \
  xtask/src/lint_invariants_api_language.rs
git commit -m "refactor: move zone reclamation to intrusive epoch bags"
```

## Task 6: Close SMP Maintenance And CPU Offline

**Files:**
- Modify: `crates/tx-hal/src/lib.rs`
- Modify: active board IPI implementations
- Modify: `crates/tx-substrate/src/epoch/domain.rs`
- Modify: `crates/tx-substrate/src/epoch/local.rs`
- Modify: `crates/tx-kernel/src/trap.rs`
- Modify: `crates/tx-kernel/src/init.rs`
- Test: `crates/tx-substrate/tests/ap_init.rs`
- Test: kernel/board IPI tests adjacent to existing IPI coverage

- [x] **Step 1: Write two-CPU membership and transfer tests**

Simulate an advancer racing AP admission, a blocked ring reuse requesting a
remote drain, and CPU offline transferring all six heads (three Zone plus three
generic). Assert membership-version changes force scan restart and no head is
overwritten.

- [x] **Step 2: Add maintenance transport**

Add `IpiKind::Maintenance`, a per-CPU drain-request bit, board send/pending/ack
support, and kernel trap dispatch that acknowledges the IPI and schedules local
epoch maintenance. The coordinator never locks or mutates a remote bag.

- [x] **Step 3: Add membership version and draining admission**

Serialize BSP/AP admission and offline state changes with the domain membership
lock. Advance snapshots the version, scans online initialized CPUs, and retries
when the version changes. Reader and retire admission reject a draining CPU.

- [x] **Step 4: Implement ordered CPU offline transfer**

Do not hold the membership lock while waiting for `local_epoch == 0`,
`retire_active == false`, and pin quiescence. Reacquire it to freeze and merge
matching-tag bags into the coordinator, then remove the CPU and bump the version.

- [x] **Step 5: Wire idle and timer maintenance**

At the existing secondary reactor idle point, process local drain requests
before generic Zone maintenance. Keep epoch bag draining separate from the
registry-wide Zone maintenance scan.

- [x] **Step 6: Verify SMP behavior**

Run:

```bash
cargo test -p tx-substrate --test ap_init
cargo test -p tx-substrate --test epoch epoch_advance_restarts_on_membership_change
cargo test -p tx-substrate --test epoch cpu_offline_transfers_all_bag_heads
cargo xtask full-build --target rv64-qemu --no-image
```

Expected: host SMP simulations pass and RV64 kernel build completes.

Host and board verification passes: epoch 23/23, AP init 4/4, Zone 18/18,
RV64 board 93/93, LA64 board 53/53, and M1Dock 14/14. The RV64 full-build
reaches `tx-reactor` and remains blocked by the shared dirty tree's existing
nine timer/task API errors (`wake::timer`, deadline registrar/current poll
mailbox hooks, `TaskGeneration::from_raw`, and parked-owner wake). The Task 6
substrate and board crates compile independently, and two review rounds closed
all P1/P2 findings.

- [ ] **Step 7: Commit SMP closure**

Deferred in the shared dirty checkout: staging and commit were not authorized.

```bash
git add crates/tx-hal boards crates/tx-substrate/src/epoch \
  crates/tx-substrate/tests crates/tx-kernel/src/trap.rs crates/tx-kernel/src/init.rs
git commit -m "feat: close epoch maintenance and cpu offline"
```

## Task 7: Implement Backend-Only Published<T>

**Files:**
- Create: `crates/tx-substrate/src/publication/mod.rs`
- Create: `crates/tx-substrate/src/publication/node.rs`
- Modify: `crates/tx-substrate/src/lib.rs`
- Test: `crates/tx-substrate/tests/publication.rs`

- [x] **Step 1: Write allocation, rollback, ordering, and Drop tests**

Cover `try_new` allocation failure, `prepare_replace` allocation failure,
uncommitted reservation rollback, one Acquire read, commit visibility, old-root
grace delay, concurrent reader/writer, exclusive Drop, and no post-swap error.

- [x] **Step 2: Add the exact backend surface**

```rust
pub struct Published<T> { /* private */ }
pub struct PublishReservation<'a, T> { /* private */ }

pub enum PublishError {
    Allocation,
}

impl<T> Published<T> {
    pub fn try_new(initial: T) -> Result<Self, PublishError>;
    pub fn read<'g>(&'g self, guard: &'g Guard<'_>) -> &'g T;
    pub fn prepare_replace(
        &self,
        next: T,
    ) -> Result<PublishReservation<'_, T>, PublishError>;
}

impl<T> PublishReservation<'_, T> {
    pub fn commit(self);
}
```

`PublishedNode<T>` contains the private 16-byte `RcuHead` followed by `T`.
Allocation and initialization complete before the writer claim is transferred
into the reservation.

Allocate `PublishedNode<T>` with an explicitly checked `alloc::alloc::alloc`
and `Layout::new::<PublishedNode<T>>()`; map a null return to
`PublishError::Allocation`, initialize with `ptr::write`, and pair it with the
matching `dealloc`. Do not implement this contract with `Box::new`, because
that cannot supply the required fallible allocation result in this workspace.

- [x] **Step 3: Make commit a non-fallible linear sequence**

Commit acquires `LocalRetireGuard`, swaps the root with Release/AcqRel ordering,
samples the post-swap epoch, enqueues the old head in `O(1)`, releases local
exclusion, and releases the private writer claim. It cannot call drain.

- [x] **Step 4: Add an unbounded intrusive post-grace drop queue**

Epoch reclaim of a published node must be `O(1)`: repurpose the detached
node's `RcuHead.next`, change its private callback to `drop_node::<T>`, and push
it to a per-CPU publication-owned deferred-drop list under a short
`LocalRetireGuard`. Maintenance detaches at most its budget and drops nodes
outside local exclusion. This removes fixed queue
capacity and inline-overflow fallback. A destructor may still have type-specific
linear cost; record that separately instead of hiding it in epoch drain stats.

- [x] **Step 5: Verify publication semantics**

Run:

```bash
cargo test -p tx-substrate --test publication
cargo test -p tx-substrate --test epoch
cargo xtask lint invariants
```

Expected: all tests pass; exported API contains only `Published`,
`PublishReservation`, and `PublishError`; no `RcuHead` or bag type is public.

The root exports exactly those three types, with `T: Send + 'static` required
because deferred values may outlive the owner and move across CPUs during
offline. Commit preflights both the sampled epoch and the one possible in-flight
advance before the root swap; the post-swap path only samples and links. Epoch
callbacks carry the existing retire guard, so Draining CPUs can finish
callback-owned nested retirement without reopening general admission. The
publication queue detaches and drops one node at a time, preserving the tail
across destructor unwind. Publication 15/15, epoch 23/23, Zone 18/18, doc
compile-fail 2/2, substrate check, formatting, diff-check, and the API-language
ratchet pass.

- [ ] **Step 6: Commit publication**

Deferred in the shared dirty checkout: staging and commit were not authorized.

```bash
git add crates/tx-substrate/src/publication crates/tx-substrate/src/lib.rs \
  crates/tx-substrate/tests/publication.rs
git commit -m "feat: add bounded intrusive publication backend"
```

## Task 8: Migrate RecipeIndex Without Changing AddressSpace Language

**Files:**
- Modify: `crates/tx-subsystems/src/vm/structure/recipe.rs`
- Modify: `crates/tx-subsystems/src/vm/structure/address_space.rs`
- Modify: `crates/tx-subsystems/src/vm/structure/recipe_tree.rs`
- Modify: `crates/tx-subsystems/src/vm/mod.rs` only if private module wiring changes
- Test: existing VM unit tests in the same files

- [x] **Step 1: Write behavior-equivalence tests around the owner API**

Test lookup during concurrent replacement, rollback on next-root allocation
failure, old-root survival under Guard, map/unmap/protect equivalence, and no
`Published` type in any `AddressSpace`-visible signature. The replacement
witness uses a barrier-controlled reader/writer overlap; allocation injection
proves rollback leaves both the recipe root and owner stats unchanged. Existing
map/unmap/protect tests remain the behavior-equivalence baseline.

- [x] **Step 2: Replace manual root ownership**

Change only the private field and helpers:

```rust
pub(in crate::vm) struct RecipeIndex {
    current: Published<RecipeTree>,
    mutation: VmSpinMutex<()>,
}

fn pinned<'g>(&'g self, guard: &'g Guard<'_>) -> &'g RecipeTree {
    self.current.read(guard)
}
```

Delete manual `Box::into_raw`, `AtomicPtr::swap`, `retire_published_tree`, and
the fixed-capacity deferred reclaim ring. Keep the semantic mutation lock over
current-read, rewrite, and commit so concurrent writers cannot publish stale
derived roots.

- [x] **Step 3: Use prepared publication in each rewrite**

Build the immutable next tree, call `prepare_replace(next)` while the mutation
claim still protects the authoritative current, then commit. Map pre-commit
allocation failure into the existing `VmMapError::NoFreeRange` / `ENOMEM`
terminal class; no new owner-visible error vocabulary is added. Commit has no
error branch.

- [x] **Step 4: Remove repeated-root lookup from range validation**

Replace `range_is_fully_mapped` with one ordered `for_each_overlapping` walk.
Track a cursor and reject the first gap. This changes the validation bound from
`O(k * h)` to `O(h + k)` without changing mapping semantics.

- [x] **Step 5: Preserve backend and node allocation choices**

Keep the current default Recipe backend and private `Arc` nodes. Do not add Zone
handles to child links. B+ default selection remains a separate performance
decision after the RCU correctness pilot.

- [x] **Step 6: Verify VM and full host behavior**

Run:

```bash
cargo test -p tx-subsystems --lib vm::structure::recipe
cargo test -p tx-subsystems --lib vm::structure::recipe_tree
cargo test -p tx-subsystems --lib vm::
cargo -q xtask unit
```

Expected: behavior-equivalence tests pass; `rg` finds no raw AtomicPtr root or
`epoch::retire_raw` in VM.

The focused Recipe and RecipeTree slices, full VM 176/176 suite, and
`cargo -q xtask unit` now execute successfully. Static shape, rustfmt,
diff-check, publication 15/15, epoch 21/21 after compatibility removal, the
expanded 20-root API-language gate, and its 53 RCU ratchet tests pass.

- [ ] **Step 7: Lower the raw-publication ratchet and commit**

The raw-retire ceiling is now zero, production roots contain no raw site, and
same-file `extern crate tx_substrate as ...` re-export aliases are covered.
The scan now covers all 20 production crate/board roots; the original five
upper roots retain strict cross-file relative-alias analysis. Staging and commit
remain deferred because they were not authorized in the shared dirty checkout.

```bash
git add crates/tx-subsystems/src/vm/structure xtask/src/lint_invariants_api_language.rs
git commit -m "refactor: publish vm recipes through substrate rcu"
```

## Task 9: Remove Compatibility Retirement And Close The Migration

**Files:**
- Delete: `crates/tx-substrate/src/epoch/retired.rs`
- Modify: `crates/tx-substrate/src/epoch/{domain,local,mod}.rs`
- Modify: `crates/tx-substrate/src/bus/{owner,macros,mod}.rs`
- Modify: `crates/tx-substrate/tests/{epoch,bus,zone}.rs`
- Modify: `crates/tx-test-support/src/adapter.rs`
- Modify: epoch drain callsites that depend on obsolete pool stats
- Modify: `xtask/src/lint_invariants_api_language.rs`
- Modify: `docs/progress/plans/2026-07-19-rcu-path-migration.json`
- Modify: `docs/progress/STATUS.md`

- [x] **Step 1: Retire the unused raw bus owner-storage API**

The current production tree has no `WireOwnerManifest` implementation outside
tests. Remove `retire_owner_storage`, the manifest/macro surface, and tests that
exist only to exercise raw callback retirement. Keep bus terminal/drain
semantics; do not invent a public generic retire carrier.

- [x] **Step 2: Delete fixed descriptors and failure vocabulary**

Remove `RetiredNode`, `PerCpuRetiredPool`, `RETIRED_NODE_POOL_CAPACITY`,
`RetiredNodePoolExhausted`, and `epoch::retire_raw`. Replace summary fields with
per-CPU bag and deferred-drop counts that do not expose bag phases.

- [x] **Step 3: Make the raw-retire ratchet zero**

Allow raw pointers and `RcuHead` only inside epoch, Zone, and publication
implementation files. Reject any future `AtomicPtr` publication root or raw
retire callback in upper owners.

- [x] **Step 4: Run the complete verification ladder**

Run:

```bash
cargo test -p tx-substrate --test epoch
cargo test -p tx-substrate --test zone
cargo test -p tx-substrate --test publication
cargo test -p tx-substrate --test bus
cargo test -p tx-subsystems --lib vm::
cargo -q xtask unit
cargo xtask lint docs
cargo xtask lint invariants
cargo xtask progress validate
cargo xtask full-build --target rv64-qemu --no-image
git diff --check
```

Expected: all commands pass. If the dirty checkout has an unrelated compile
blocker, record the exact failing crate and still run every narrower gate that
reaches the changed code.

All RCU-scoped, build, host-unit, docs, progress, full-build, formatting, and
diff gates pass. The aggregate `cargo xtask lint invariants` command still
fails on pre-existing unrelated dirty-tree ratchets: STEP stage comments,
SUBJ ignored contexts, the `sys_openat` cred gate, the VFS `fd_ready`
guard-field rule, notification-boundary, and syscall-no-await. The ratcheted
RCU/API-language sub-gate passes with zero RCU findings and 403 report-only
adapter/wait findings.

- [x] **Step 5: Run the SMP guest witness**

Run the repository's bounded RV64 SMP smoke path after `full-build`. The witness
must show at least two initialized epoch CPUs, concurrent guarded Recipe reads,
remote maintenance acknowledgement, bounded drains, and no reclaim before
quiescence.

The bounded RV64 smoke runs with `--smp 4 --timeout-ms 30000` and requires six
RCU markers. An AP task holds a real outer Guard in one synchronous poll while
the BSP tags the Recipe root. The BSP observes zero early bag reclaim and
publication drop, receives a real Maintenance IPI ACK, releases the reader,
then drains with budget one until both the retired root callback and deferred
destructor complete.

- [x] **Step 6: Close progress; commit remains deferred**

Mark every completed plan step, record verification and residual destructor
costs, update `STATUS.md`, and validate progress JSON before committing.

Progress is closed and validated. Staging and commit remain intentionally
deferred because the shared dirty checkout has no explicit commit authorization.

```bash
git add crates/tx-substrate crates/tx-subsystems/src/vm crates/tx-hal \
  crates/tx-kernel boards xtask docs/progress
git commit -m "refactor: retire fixed pool ebr compatibility"
```

## 3. Landing Gates

1. **Reader gate:** Task 2 must pass before any owner uses Zone-backed links in
   a guarded read path.
2. **Exclusion gate:** Task 3 must pass before intrusive enqueue is enabled in
   the default build.
3. **Bounded-retire gate:** Tasks 4-6 must pass before `Published<T>` is exposed
   to another crate.
4. **Pilot gate:** Task 7 must pass before RecipeIndex migration starts.
5. **Expansion gate:** Task 9 and the SMP witness must pass before migrating any
   candidate beyond VM.

The first expansion candidate after closure is PageContainer's immutable
resident lookup root. It is not part of this plan.
