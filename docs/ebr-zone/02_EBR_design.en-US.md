# Foundation Layer Detailed Design - Section 2: EBR (Epoch-Based Reclamation)

**Date:** 2026-04-19
**Adapted:** 2026-04-27 to the txKernel object-model interface.
**Code location:** `tx-kernel/substrate/epoch/` or equivalent foundation crate module.
**Depends on:** HAL `PercpuIf` / current CPU ID, substrate synchronization primitives, and timer/pressure trigger points for `try_drain`.
**Depended on by:** `substrate/zone` slot reclamation trigger points and upper kernel hot-path traversals through `IdentRef<'g, T>`.

**Interface status:** adapted to the txKernel object-model interface. Public
callers hold `Guard` through `epoch::guard()`. `Weak<T>` is a generation-tagged,
non-retaining handle; it is not a weak refcount and does not participate in
semantic reclamation. `Cap<T>` / payload evidence carry retention, while EBR
protects `IdentRef<'g, T>` traversal and delays physical reuse.

**Policy-zone decision:** EBR is one hidden policy family under the universal
policy-based zone substrate. Upper subsystems name role-shaped types
(`Cap<T>`, `PayloadCap<T>`, `Weak<T>`, witnesses carrying `IdentRef<'g, T>`,
identity slots, projection rows); they do not pass `Zone<T, EbrPolicy>` through
operation code.

**Executable implementation update (2026-07-21):** the `RetiredNode` /
per-CPU linked-list pseudocode retained later in this historical detailed
walkthrough is superseded by the active contract in
`docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`. The implementation now
follows Crossbeam's collection shape: 64 callbacks per CPU-local bag, a SeqCst
fence before sealing with the global epoch, a growable FIFO of page-backed
sealed bags, collection every 128 pinnings, at most eight bags per periodic
pass, and the same two-epoch safety margin. Empty bag caching is bounded; there
is no fixed 1024-node retirement ceiling. Guard entry also follows Crossbeam's
balanced nesting rule: a per-CPU depth counter publishes the epoch and advances
the periodic pin counter only on `0 -> 1`, and clears the epoch only on
`1 -> 0`. The older list snippets below explain the safety argument but are not
the current storage algorithm.

---

## 0. Background: Why EBR Is Needed

One of this kernel's core designs is the separation of **identity** from **payload**. Two examples show the challenge this creates:

**Zombie processes:** after a process exits, its address space and fd table have already been released (payload gone), but its pid and exit code must remain (identity alive) until the parent reaps it with `waitpid`.

**Files that are unlinked but still open:** after `unlink`, the file disappears from the directory tree (namespace binding removed), but as long as some process still holds an fd, the file data (page cache) must not be released.

This layered death model requires hot paths (path lookup, fd lookup, pid lookup) to read identity fields quickly **without holding a reference count**. But if readers do not hold reference counts, the system needs another mechanism to guarantee that "the memory I am reading will not be freed while I read it."

**EBR is that mechanism**: readers do not "grab" objects. Instead, writers wait until all readers have left before freeing memory.

The concrete target protected by EBR in this system is: **slab pages are not returned to the frame allocator**. Readers only need the slab page memory address to remain valid. Whether the object in a slot has already been destructed, or whether the generation has changed, is judged by readers themselves through generation checks.

The responsibility boundary is important: **EBR only delays physical reclamation. It does not decide when an object's semantic lifetime ends.** For co-located objects, these two points are often close. For split entities, they are usually two different commit points.

---

## 1. Global Architecture

EBR consists of the following pieces:

```text
epoch/
|- mod.rs          <- public interface: guard() / retire() / try_drain()
|- domain.rs       <- EpochDomain: owns the global epoch counter
|- guard.rs        <- Guard: RAII guard, leaves the epoch on drop
|- local.rs        <- CpuLocalEpochState: per-CPU local state
`- retired.rs      <- RetiredNode / RetiredList: pending-reclamation list
```

Overall relationship:

```text
EpochDomain
|
|- global_epoch: AtomicU64          <- global monotonically increasing counter
|
`- cpu_states: CpuLocal<CpuLocalEpochState>
   |
   |- local_epoch: AtomicU64        <- current CPU epoch (0 = not in any epoch)
   |- pin_depth: AtomicUsize        <- balanced same-CPU Guard nesting depth
   `- retired: RetiredList          <- this CPU's pending-reclamation list
      |- head: *mut RetiredNode
      `- count: usize
```

---

## 2. Complete Data Structure Definitions

### 2.1 EpochDomain (`domain.rs`)

```rust
/// Global EBR manager. This system has exactly one GLOBAL_DOMAIN instance.
pub struct EpochDomain {
    /// Global epoch counter. Starts from 1.
    /// 0 is reserved to mean "not in any epoch".
    /// Monotonically increases; u64 will not overflow in practice.
    global_epoch: CachePadded<AtomicU64>,

    /// Per-CPU local state, accessed through CpuLocal.
    cpu_states: CpuLocal<CpuLocalEpochState>,
}

/// CachePadded is a wrapper that pads the inner field to a 64-byte boundary
/// to prevent false sharing between CPUs on the same cache line.
struct CachePadded<T> {
    value: T,
    _pad: [u8; 64 - size_of::<T>()],
}
```

Global singleton:

```rust
// epoch/mod.rs
static GLOBAL_DOMAIN: EpochDomain = EpochDomain::new();

pub fn guard() -> Guard<'static> {
    GLOBAL_DOMAIN.guard()
}

pub unsafe fn retire<T>(slot_ptr: *mut Slot<T>, reclaim_fn: unsafe fn(*mut u8)) {
    GLOBAL_DOMAIN.retire(slot_ptr as *mut u8, reclaim_fn);
}

pub fn try_drain() {
    GLOBAL_DOMAIN.try_drain();
}
```

### 2.2 CpuLocalEpochState (`local.rs`)

```rust
/// Per-CPU EBR local state. Accessed through CpuLocal<> and does not need a lock.
pub(crate) struct CpuLocalEpochState {
    /// Which epoch this CPU is currently in.
    /// - 0: no Guard is held; not in any epoch
    /// - N > 0: a Guard is held; the global epoch was N on entry
    ///
    /// This is AtomicU64 rather than a plain u64 because try_advance_epoch()
    /// reads this field from other CPUs (read-only, not written remotely).
    pub(crate) local_epoch: AtomicU64,

    /// Number of guards currently nested on this CPU. The local epoch is
    /// published only for 0 -> 1 and cleared only for 1 -> 0.
    pub(crate) pin_depth: AtomicUsize,

    /// This CPU's pending-reclamation list.
    /// Only this CPU writes the list, so no lock is required.
    pub(crate) retired: RetiredList,
}

impl CpuLocalEpochState {
    pub(crate) const fn new() -> Self {
        Self {
            local_epoch: AtomicU64::new(0),
            pin_depth: AtomicUsize::new(0),
            retired: RetiredList::new(),
        }
    }
}
```

### 2.3 RetiredList and RetiredNode (`retired.rs`)

```rust
/// List of objects pending reclamation.
pub(crate) struct RetiredList {
    /// List head pointer. Raw pointer because retire nodes are substrate-owned
    /// sidecars and are linked without allocation on the hot path.
    head: *mut RetiredNode,
    /// List length, used to decide whether an early drain should be triggered.
    count: usize,
}

impl RetiredList {
    pub(crate) const fn new() -> Self {
        Self { head: ptr::null_mut(), count: 0 }
    }

    /// Push a node to the list head. O(1).
    pub(crate) fn push(&mut self, node: *mut RetiredNode) {
        unsafe {
            (*node).next = self.head;
        }
        self.head = node;
        self.count += 1;
    }

    /// Pop the list head. O(1). None means the list is empty.
    pub(crate) fn pop(&mut self) -> Option<*mut RetiredNode> {
        if self.head.is_null() {
            return None;
        }
        let node = self.head;
        self.head = unsafe { (*node).next };
        self.count -= 1;
        Some(node)
    }

    pub(crate) fn is_empty(&self) -> bool { self.head.is_null() }
    pub(crate) fn count(&self) -> usize { self.count }
}

/// One node in the pending-reclamation list.
///
/// RetiredNode must be available without dereferencing a reclaimed object's
/// semantic fields. In the txKernel interface, EBR-observed slot contents may
/// still be read through pre-existing `IdentRef<'g, T>` values until the guard
/// drops, so the generic retained-entity policy must not overwrite T's data
/// area with a RetiredNode before epoch quiescence.
///
/// Implementations may store this node in a metadata-reserved sidecar area, a
/// per-CPU retire-node pool, or in slot storage only for policies that prove
/// the value has no possible guarded readers after retirement. The public
/// contract is allocation-bounded retirement, not "always embed in T".
#[repr(C)]
pub(crate) struct RetiredNode {
    /// Pointer to the slot or data area being retired.
    /// The reclaim callback uses this pointer to recover metadata, run the
    /// destructor, increment generation, and return the slot.
    pub(crate) slot_ptr: *mut u8,

    /// Reclamation callback supplied by Zone when it calls retire().
    /// It performs: generation++, state = Free, and returns the slot to Zone.
    pub(crate) reclaim_fn: unsafe fn(*mut u8),

    /// Global epoch value when this object was retired.
    /// try_drain uses this value to decide whether it is safe to reclaim
    /// (all CPUs have passed this epoch).
    pub(crate) retired_at_epoch: u64,

    /// List next pointer.
    pub(crate) next: *mut RetiredNode,
}
```

**Sidecar diagram:**

```text
slot memory layout (in Retiring state):

+-------------------------------------------+
| SlotMeta                                  | <- generation/retain/state
+-------------------------------------------+
| alignment padding                         |
+-------------------------------------------+
| T data                                    | <- not overwritten until safe
+-------------------------------------------+

retired sidecar:
  slot_ptr
  reclaim_fn
  retired_at_epoch
  next
```

### 2.4 Guard (`guard.rs`)

```rust
/// Guard: while it is held, the current CPU is in an epoch.
///
/// - !Send: cannot be sent across threads (the guard is tied to a specific CPU)
/// - !Sync: shared references are not allowed
/// - May be nested on the same CPU; nested guards share the outermost epoch
///   publication and are balanced by the per-CPU pin depth
pub struct Guard {
    /// Pointer to this CPU's CpuLocalEpochState at epoch entry.
    /// drop uses it to decrement pin_depth and, on the final drop, clear
    /// local_epoch.
    cpu_state: *mut CpuLocalEpochState,

    /// CPU pinned at entry, preventing migration while the guard is alive.
    _cpu_pin: CpuPinGuard,

    /// Disallow Send/Sync.
    _not_send: PhantomData<*mut ()>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Leave the epoch: clear local_epoch to indicate that this CPU is
        // no longer in any epoch.
        unsafe {
            (*self.cpu_state)
                .local_epoch
                .store(0, Ordering::Release);
        }
        // _cpu_pin automatically unpins the CPU when dropped.
    }
}
```

---

## 3. Detailed Implementation of Core Operations

### 3.1 `guard()` - Enter an Epoch

**Goal:** make the current CPU's `local_epoch` record the current global epoch. After that, holding raw pointers is safe.

**Memory-ordering requirements:**

- Writing `local_epoch` must become visible **before reading protected objects**.
- This lets `try_advance_epoch()` correctly see that "this CPU is still in an old epoch" and prevents premature advancement.

```rust
// domain.rs
impl EpochDomain {
    pub fn guard(&self) -> Guard {
        // Step 1: pin the current CPU to prevent migration while the guard lives.
        let cpu_pin = cpu::pin_current_cpu();
        let cpu_id = cpu_pin.id();

        // Step 2: get this CPU's local state.
        let cpu_state = self.cpu_states.get_mut_for(cpu_id);

        // Step 3: increase the balanced nesting depth.
        let previous_depth = cpu_state.pin_depth.fetch_add(1, Ordering::Relaxed);

        // Step 4: only the outermost 0 -> 1 transition publishes an epoch.
        // Nested guards reuse this already-published epoch and do not advance
        // the periodic 128-pinning counter.
        if previous_depth == 0 {
            let current_epoch = self.global_epoch.value.load(Ordering::Acquire);
            cpu_state.local_epoch.store(current_epoch, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            cpu_state.note_outermost_pin_and_maybe_collect();
        }

        Guard {
            cpu_state: cpu_state as *mut _,
            _cpu_pin: cpu_pin,
            _not_send: PhantomData,
        }
    }
}
```

**Why SeqCst?**

Consider this race:

```text
CPU0                              CPU1 (try_advance_epoch)
--------------------------------  --------------------------------
read global_epoch = 5             read CPU0 local_epoch = 0
                                  (believes CPU0 is not in any epoch)
                                  advance global_epoch to 6

                                  retire an object with epoch recorded as 5

                                  advance epoch to 7
                                  call reclaim_fn for epoch=5 object
                                  slab page is freed!

write local_epoch = 5             <- too late
start reading the freed slab page <- use-after-free
```

SeqCst gives the write `local_epoch = 5` on CPU0 and the read of `local_epoch` on CPU1 a single global order, eliminating this race window.

### 3.2 `Guard::drop()` - Leave an Epoch

```rust
// guard.rs
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            let previous_depth = (*self.cpu_state)
                .pin_depth
                .fetch_sub(1, Ordering::Relaxed);
            assert!(previous_depth > 0, "epoch guard depth underflow");

            // Only the final 1 -> 0 transition advertises quiescence. Release
            // ordering ensures all protected reads from every nested guard
            // complete before a collector can observe local_epoch = 0.
            if previous_depth == 1 {
                (*self.cpu_state)
                    .local_epoch
                    .store(0, Ordering::Release);
            }
        }
        // _cpu_pin is dropped automatically here.
    }
}
```

### 3.3 `retire()` - Add an Object to the Pending-Reclamation List

```rust
// domain.rs
impl EpochDomain {
    /// Add a slot to this CPU's pending-reclamation list.
    ///
    /// Safety requirements guaranteed by the caller:
    /// 1. The slot has already entered Retiring state (the no-upgrade
    ///    SENTINEL_DEAD barrier is installed, retention is zero, and
    ///    epoch::retire() is called exactly once).
    /// 2. The reclaim callback may run only after all guards that could hold
    ///    IdentRef<'g, T> values have quiesced.
    pub unsafe fn retire(&self, slot_ptr: *mut u8, reclaim_fn: unsafe fn(*mut u8)) {
        // Step 1: allocate or select a RetiredNode sidecar. A concrete
        // implementation may use a metadata-reserved sidecar or a bounded
        // per-CPU retire-node pool. Do not overwrite T's data area for
        // generic retained-entity slots before epoch quiescence.
        let node = alloc_retired_node();
        (*node).slot_ptr = slot_ptr;
        (*node).reclaim_fn = reclaim_fn;

        // Step 2: record the current global epoch. Relaxed is sufficient:
        // this is just a timestamp.
        (*node).retired_at_epoch = self.global_epoch.value.load(Ordering::Relaxed);
        (*node).next = ptr::null_mut();

        // Step 3: push to this CPU's retired list.
        // No lock is needed because only this CPU mutates the list.
        // We still pin the CPU to prevent migration during the operation.
        let cpu_pin = cpu::pin_current_cpu();
        let cpu_state = self.cpu_states.get_mut_for(cpu_pin.id());
        cpu_state.retired.push(node);

        // Step 4: if the list has grown too much, try to drain it.
        if cpu_state.retired.count() > RETIRE_THRESHOLD {
            // Do not call try_drain before dropping cpu_pin because try_drain
            // pins the CPU internally.
            drop(cpu_pin);
            self.try_drain();
        }
    }
}

/// Try an early drain when a per-CPU retired list exceeds this size.
const RETIRE_THRESHOLD: usize = 64;
```

### 3.4 `try_advance_epoch()` - Try to Advance the Global Epoch

```rust
// domain.rs
impl EpochDomain {
    /// Try to advance the global epoch from N to N+1.
    ///
    /// Advancement condition: every CPU's local_epoch is either 0
    /// (not in any epoch) or equal to the current global epoch (already current).
    /// If any CPU has 0 < local_epoch < current global epoch, it is still in
    /// an old epoch and advancement is not allowed.
    ///
    /// Returns whether advancement succeeded.
    fn try_advance_epoch(&self) -> bool {
        // Read the current global epoch.
        let cur = self.global_epoch.value.load(Ordering::Acquire);

        // Check all CPUs' local_epoch values.
        for cpu_id in cpu::all_cpu_ids() {
            let state = self.cpu_states.get_for(cpu_id);
            let local = state.local_epoch.load(Ordering::Acquire);

            // local == 0: this CPU is not in any epoch.
            // local == cur: this CPU is already in the current epoch.
            // local < cur and local != 0: this CPU is still in an old epoch.
            if local != 0 && local < cur {
                return false;
            }
        }

        // All CPUs satisfy the condition. CAS global_epoch from cur to cur+1.
        // AcqRel:
        //   - Acquire: observe all local_epoch writes
        //   - Release: ensure reclaim_fn observes writes before cur+1
        self.global_epoch
            .value
            .compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        // If CAS fails, another CPU advanced first. That is fine.
    }
}
```

### 3.5 `try_drain()` - Reclaim Expired Objects

```rust
// domain.rs
impl EpochDomain {
    /// Try to advance the epoch, then reclaim safe objects in this CPU's
    /// retired list.
    ///
    /// Called at:
    /// 1. the end of timer interrupt handling (once per CPU per tick)
    /// 2. retire() when the retired list exceeds the threshold
    ///
    /// Guaranteed bounded work: process at most DRAIN_BATCH objects each time.
    pub fn try_drain(&self) {
        // Step 1: try to advance the epoch.
        self.try_advance_epoch();
        // Continue even if advancement fails: older objects may already be safe.

        // Step 2: read the current global epoch.
        let safe_epoch = self.global_epoch.value.load(Ordering::Acquire);

        // Step 3: pin this CPU and operate on its retired list.
        let cpu_pin = cpu::pin_current_cpu();
        let cpu_state = self.cpu_states.get_mut_for(cpu_pin.id());

        // Step 4: traverse the retired list and reclaim expired objects.
        // "Expired" means: retired_at_epoch + 2 <= safe_epoch.
        //
        // Why +2?
        // An object retired at epoch N must wait until the epoch advances to
        // N+2 before we can ensure every CPU has "seen" this retire operation
        // and no CPU still holds a raw pointer from epoch N.
        let mut drained = 0;
        let mut remaining_head: *mut RetiredNode = ptr::null_mut();
        let mut remaining_tail: *mut RetiredNode = ptr::null_mut();

        while let Some(node) = cpu_state.retired.pop() {
            if drained < DRAIN_BATCH
                && safe_epoch >= unsafe { (*node).retired_at_epoch } + 2
            {
                // Safe to reclaim.
                unsafe { ((*node).reclaim_fn)((*node).slot_ptr) };
                drained += 1;
            } else {
                // Not ready yet, or the batch limit has been reached.
                // Put it into the remaining list.
                unsafe {
                    (*node).next = ptr::null_mut();
                    if remaining_tail.is_null() {
                        remaining_head = node;
                        remaining_tail = node;
                    } else {
                        (*remaining_tail).next = node;
                        remaining_tail = node;
                    }
                }
            }
        }

        // Attach the remaining list back to retired list.
        if !remaining_head.is_null() {
            cpu_state.retired.head = remaining_head;
            // Recounting is simplified here; a real implementation can maintain count.
        }
    }
}

/// Maximum number of objects processed per drain.
const DRAIN_BATCH: usize = 32;
```

---

## 4. How Zone Connects to EBR

Boundary notes:

- `Cap<T>` / `PayloadCap<T>` / typed operational evidence reduce retention.
- `Weak<T>` does **not** reduce retention on drop. It is a generation-tagged,
  non-retaining handle.
- Witnesses, identity-slot reads, and projection rows are the upper-language
  places where EBR appears. They expose `IdentRef<'g, T>` or a
  projection-specific guard-scoped wrapper, not a raw EBR policy parameter.
- `epoch::retire()` delays a reclaimable slot until after the epoch-safe
  window, then invokes the zone reclaim callback.
- For split-lifetime entities, identity retention is held by
  `Cap<Identity>` and payload retention by `PayloadCap<Payload>` or a typed
  contribution. Authoritative addressability indexes must not use `Weak<T>`.

Therefore, the Zone/EBR interface shape is: final retention drop installs the
no-upgrade barrier (`SENTINEL_DEAD` / dead state), then calls `try_retire()`.
`try_retire()` atomically claims `Dead -> Retiring`, guaranteeing
`epoch::retire()` is called exactly once for that lifecycle.

### 4.1 `try_retire()`: The Unified Entry Point into EBR

```rust
// zone/cap.rs
fn try_retire<T>(data_ptr: *mut u8) {
    let meta = meta_ptr_from_data::<T>(data_ptr);
    loop {
        let cur = unsafe { (*meta).load(Ordering::Acquire) };

        // Enter EBR only if final retention is gone and the no-upgrade
        // barrier has already been installed.
        if cur.retain() != 0 || cur.state() != SlotState::Dead {
            return;
        }

        // CAS Dead -> Retiring: atomically claim enqueue ownership.
        let new = cur.with_state(SlotState::Retiring);
        match unsafe { (*meta).cas(cur, new, Ordering::AcqRel, Ordering::Acquire) } {
            Ok(_) => {
                unsafe { epoch::retire(data_ptr, reclaim_slot::<T>) };
                return;
            }
            Err(_) => continue,
        }
    }
}
```

### 4.2 Final `Cap<T>` Drop

```rust
// zone/cap.rs
impl<T> Drop for Cap<T> {
    fn drop(&mut self) {
        let meta = self.slot_meta();

        // Decrement identity retention.
        let old = meta.fetch_dec_retain(Ordering::AcqRel);

        if old.retain() > 1 {
            return;
        }

        // This is the final identity-retaining Cap. Install the no-upgrade
        // barrier. Generic retained-entity slots do not destruct T here,
        // because pre-existing IdentRef<'g, T> readers may still exist.
        meta.set_dead(Ordering::Release);

        try_retire::<T>(self.slot_data_ptr::<T>());
    }
}
```

Payload slots follow the same shape, except the counter is payload-specific:
`PayloadCap<T>` or a typed contribution decrements its declared payload
counter; the final drop withdraws operational availability and queues payload
retirement.

### 4.3 `Weak<T>` Drop

There is no `Weak<T>` drop path into reclamation:

```rust
impl<T> Drop for Weak<T> {
    fn drop(&mut self) {
        // no-op: Weak<T> is not retention evidence
    }
}
```

`Weak<T>::observe(&Guard)` may fail because the slot has been retired, reused,
or generation-mismatched. That is the intended behavior for resolution-only
bindings and stale-tolerant caches.

### 4.4 `reclaim_slot`: Reclamation Callback

```rust
// zone/slot.rs
/// Actual slot reclamation after the epoch advances.
/// At this point, no Guard can still protect an IdentRef into this slot.
unsafe fn reclaim_slot<T: ZoneAllocated>(data_ptr: *mut u8) {
    let meta_ptr = meta_ptr_from_data::<T>(data_ptr);
    let slot_id  = slot_id_from_meta::<T>(meta_ptr);
    let zone_id  = zone_id_for::<T>();

    // Now it is safe to run T's destructor for EBR-observed slot contents.
    ptr::drop_in_place(data_ptr as *mut T);

    // Increment generation and set state back to Free.
    // From this point:
    //   - old Weak<T> observes generation mismatch or absence and fails
    //   - old IdentRef cannot exist because the epoch has advanced
    loop {
        let cur = (*meta_ptr).load(Ordering::Acquire);
        debug_assert_eq!(cur.state(), SlotState::Retiring);
        debug_assert_eq!(cur.retain(), 0);
        let new = cur.inc_gen().with_state(SlotState::Free);
        if (*meta_ptr).cas(cur, new, Ordering::Release, Ordering::Relaxed).is_ok() {
            break;
        }
    }

    zone_for::<T>(ZoneId(zone_id)).return_slot(slot_id);
}
```

---

## 5. Full Slot Lifecycle (Including EBR)

```text
  zone::reserve()
      |
      v
  +----------+
  | Reserved |  held by ZoneReservation<T>, retain=0, state=Reserved
  +----+-----+
       |
       | zone::sign(reservation, value)
       v
  +------+
  | Live |  returns Cap<T>, retain=1, state=Live
  +--+---+
     |
     | Cap::clone()       Cap::downgrade()
     | retain++           returns Weak<T>, no counter change
     |
     | final Cap::drop()
     | retain=0
     | state = Dead / SENTINEL_DEAD
     v
  +------+
  | Dead |  no new Cap upgrades can succeed
  +--+---+
     |
     | try_retire() (Dead -> Retiring CAS)
     v
  +----------+
  | Retiring |  in EBR queue; T data remains intact for old IdentRefs
  +----+-----+
       |
       | global_epoch >= retired_at_epoch + 2
       | try_drain() runs reclaim_fn
       v
  +------+
  | Free |  T::drop has run; generation++; slot returned to Zone
  +------+
       |
       `-> zone::reserve() may allocate this slot again
           (generation changed; old Weak<T> is invalid)
```

---

## 6. When `try_drain()` Is Called

```text
timer interrupt path:

  trap_handler()
    -> reactor::handle_timer_interrupt()
      -> scheduler::tick()         <- scheduler-related work
      -> epoch::try_drain()        <- each CPU drains its own retired list every tick
      -> return_to_user()
```

Besides timer ticks, `try_drain` is also triggered when:

- `retire()` finds that the current CPU's retired list exceeds
  `RETIRE_THRESHOLD` and immediately tries one drain.
- Allocation pressure asks the zone allocator to synchronously drain bounded
  work before reporting exhaustion.

---

## 7. Cooperation Between IdentRef and Guard

`IdentRef<'g, T>` is not defined in the EBR module. It is defined in `zone/`
or by upper-level users, but its safety depends on the lifetime of `Guard`.

```rust
// Skeleton of IdentRef; the real definition lives in zone/.
pub struct IdentRef<'g, T> {
    /// Raw pointer to T's data area inside a slot.
    ptr: *const T,
    /// Lifetime binding: IdentRef cannot outlive Guard.
    _guard: PhantomData<&'g Guard>,
}

impl<'g, T> IdentRef<'g, T> {
    pub unsafe fn from_raw(ptr: *const T, _guard: &'g Guard) -> Self {
        Self { ptr, _guard: PhantomData }
    }

    pub fn get(&self) -> &T {
        unsafe { &*self.ptr }
    }
}
```

Typical hot-path usage:

```rust
fn lookup_dentry(name: &str) -> Option<u64> {
    let g = epoch::guard();
    let weak = dentry_cache.lookup(name)?;
    let ident = weak.observe(&g)?;

    let data = ident.get();
    if data.is_dead() {
        return None;
    }

    Some(data.ino)
}
```

If the operation needs to carry the result beyond the step, it upgrades:

```rust
let cap: Cap<DEntry> = ident.to_cap()?;
```

---

## 8. Public Interface Summary (`epoch/mod.rs`)

```rust
// Public external interfaces.

/// Enter an epoch and return Guard.
/// While the guard is alive, retired slab pages are guaranteed not to be freed.
/// Balanced same-CPU nesting is allowed. Nested guards reuse the epoch
/// published by the outermost guard.
pub fn guard() -> Guard { GLOBAL_DOMAIN.guard() }

/// Submit a slot to this CPU's pending-reclamation list.
/// Called internally by Zone's try_retire(); not exposed to kernel users.
pub(crate) unsafe fn retire(slot_data_ptr: *mut u8, reclaim_fn: unsafe fn(*mut u8)) {
    GLOBAL_DOMAIN.retire(slot_data_ptr, reclaim_fn)
}

/// Try to advance the epoch and reclaim expired objects.
/// Called from the timer path and allocation-pressure paths. Work is bounded.
pub fn try_drain() { GLOBAL_DOMAIN.try_drain() }

pub struct Guard { ... }   // !Send, !Sync, leaves epoch automatically on drop
```

---

## 9. Directory Tree and File Responsibilities

```text
tx-kernel/substrate/epoch/
|
|- mod.rs
|    external interface: guard() / try_drain()
|    internal interface: retire() (pub(crate), only for zone/)
|    global singleton: static GLOBAL_DOMAIN: EpochDomain
|
|- domain.rs
|    struct EpochDomain { global_epoch, cpu_states }
|    impl EpochDomain { guard(), retire(), try_drain(), try_advance_epoch() }
|    const RETIRE_THRESHOLD: usize = 64
|    const DRAIN_BATCH: usize = 32
|
|- guard.rs
|    struct Guard { cpu_state, _cpu_pin, _not_send }
|    impl Drop for Guard (decrements pin_depth; final drop clears local_epoch)
|
|- local.rs
|    struct CpuLocalEpochState { local_epoch, pin_depth, local retired bag }
|    impl CpuLocalEpochState { new() }
|
`- retired.rs
     struct RetiredList { head: *mut RetiredNode, count: usize }
     impl RetiredList { push(), pop(), is_empty, count }
     struct RetiredNode { slot_ptr, reclaim_fn, retired_at_epoch, next }
     const EPOCH_SAFE_MARGIN: u64 = 2
```

---

## 10. Invariants

| ID | Invariant | Guaranteed by |
|----|-----------|---------------|
| EBR-1 | While a `Guard` is alive, retired slab pages are not freed | `try_drain`'s `+2` check plus guard publication ordering |
| EBR-2 | After `retire()` is called, readers holding `IdentRef` are protected by `Guard` and the slab page is not physically freed | Epoch advancement requires all CPUs to leave older epochs |
| EBR-3 | Old `Weak<T>` cannot upgrade successfully after reclamation | generation++ before Free; `Weak::observe` checks generation |
| EBR-4 | `try_drain` has bounded work | `DRAIN_BATCH` caps processed nodes |
| EBR-5 | `retire()` cannot deadlock on semantic locks | retired lists are substrate-owned and do not call semantic code |
| EBR-6 | `Guard` nesting is balanced on one CPU; only the outermost guard publishes and only the final drop clears the epoch | per-CPU `pin_depth` and CPU pinning |
| EBR-7 | `Guard` cannot be sent across threads | `PhantomData<*mut ()>` / CPU pinning |
| EBR-8 | IRQ handlers do not create `Guard`s | IRQ handlers must not block epoch advancement |
| EBR-9 | Split-lifetime retention is carried by `Cap<Identity>` and payload evidence, not `Weak<T>` | binding obligation derivation: addressability -> `Cap<T>`, resolution-only -> `Weak<T>` |
