# Foundation Layer Detailed Design - Section 3: Zone / Cap Object Storage

**Date:** 2026-04-19
**Adapted:** 2026-04-27 to the txKernel object-model interface.
**Code location:** `tx-kernel/substrate/zone/` or equivalent foundation crate module.
**Depends on:** `substrate/epoch` (`Guard`, delayed slot reclamation),
`PAGE_SUBSTRATE_v1` frame/slab-page allocation, and HAL `PercpuIf` for
per-CPU bucket pinning.
**Depended on by:** all zone-backed semantic objects (`ProcessIdentity`,
`ThreadIdentity`, `AddressSpace`, `RNode`, `MountIdentity`, `TtyIdentity`,
service ledger objects, and payload boxes)

---

## 0. Interface Position

This document specifies the storage machinery behind the object-model reference
hierarchy:

```text
Weak<T>
    -> observe under Guard
IdentRef<'g, T>
    -> upgrade by SENTINEL_DEAD-guarded CAS
Cap<T>
    -> upgrade payload/contribution
T::OperationalEvidence
```

The public substrate surface is:

```rust
epoch::guard() -> Guard

zone::reserve<T>(&Zone<T>) -> Result<ZoneReservation<T>, AllocError>
zone::sign<T>(ZoneReservation<T>, T) -> Cap<T>

Weak<T>
IdentRef<'g, T>
Cap<T>
PayloadCap<T>
T::OperationalEvidence
```

Policy-based zones are the universal lifetime substrate for reclaimable upper
entities. Upper subsystems do not select `Zone<T, Policy>` at call sites. They
declare entity shape and binding obligations; the zone implementation hides the
policy that realizes those obligations.

The architectural surface stays role-shaped:

| Upper role | Public type | Hidden policy |
|---|---|---|
| Owned identity | `Cap<T>` | retained slot |
| Payload use | `PayloadCap<T>` or `T::OperationalEvidence` | retained payload/contribution |
| Stale hint | `Weak<T>` | generation-checked, non-retaining |
| Witness | witness carrying `IdentRef<'g, T>` | EBR guard scope |
| Projection/read-mostly view | projection row or `ProjectionRef<'g, T>` | EBR observation plus revalidation |

---

## 1. Problems Solved

**ABA.** After a slot is reclaimed and reused, an old `Weak<T>` must not observe
or upgrade to the new object. Generation tags solve this.

**Reserve / commit.** During operations such as fork, several slots and indexes
must be reserved before anything becomes visible. `ZoneReservation<T>` is a
linear proof object with Drop rollback; `zone::sign` consumes it at commit.

**Dangling observations.** `Weak<T>::observe()` and container walks read slot
metadata and may build `IdentRef<'g, T>`. EBR ensures the slab/page backing the
slot is not physically freed while a `Guard` is alive.

**Long-lived retention.** `Cap<T>`, `PayloadCap<T>`, and typed operational
contributions hold refcounted retention. `Weak<T>` does not.

---

## 2. Strategy Table

| Mechanism | Use | Do not use | Reason |
|---|---|---|---|
| Slot metadata | packed atomic word with generation, retain, state | independent generation/state/retain atomics | upgrade must check generation + state + retain in one CAS |
| `Cap<T>` | compact slot key, identity retention | raw pointer | `Cap` must survive epochs and block semantic retirement |
| `Weak<T>` | slot key + generation snapshot, no retention | weak refcount | resolution-only evidence must not keep objects alive |
| `ZoneReservation<T>` | `!Copy`, Drop rollback | initialize-before-all-reservations | matches step reserve/commit discipline |
| `zone::sign` | infallible commit after successful reservation | fallible publish | reserve already owns storage; sign writes value and publishes |
| EBR retire | defer destructor and slot reuse past guard quiescence | immediate `T::drop` on final `Cap` | old `IdentRef<'g, T>` may still read slot contents |
| Policy choice | hidden in `Zone<T>` declaration | `Zone<T, Policy>` in upper subsystems | upper layers speak role-shaped obligations/evidence, not allocator policy |

---

## 3. Directory Tree And Responsibilities

```text
tx-kernel/substrate/zone/
|- mod.rs          <- public Zone<T> interface; zone registry; zone_for<T>
|- meta.rs         <- SlotMeta, SlotWord, SlotState
|- slot.rs         <- slot layout, pointer conversions
|- slab.rs         <- Slab<T>, bitmap allocation, back-pointer support
|- keg.rs          <- slab lists, batch refill, slab-page EBR release
|- cap.rs          <- Cap<T>, Weak<T>, IdentRef<'g, T>, upgrade/drop paths
`- reservation.rs  <- ZoneReservation<T>, sign / rollback paths
```

The module owns generic object storage. It does not know process, VFS, VM, or
device semantics.

---

## 4. SlotMeta

### 4.1 Packed Word

```text
 63                                                              0
 +---------------+-------------------------------+--------------+
 | generation    | retain_count                  |    flags     |
 |   [63:48]     |   [47:16]                     |   [15:0]     |
 |   16 bits     |   32 bits                     |   16 bits    |
 +---------------+-------------------------------+--------------+

flags[15:0]:
  [2:0]  state   - 000=Free, 001=Reserved, 010=Live, 011=Dead, 100=Retiring
  [15:3] reserved
```

Meaning:

- `generation`: incremented after each full lifecycle before returning to Free.
- `retain_count`: identity retention for retained-entity slots, or the relevant
  policy counter for payload slots.
- `state`: slot lifecycle state.

There is no `weak_count`. `Weak<T>` is not retention evidence.

### 4.2 SlotState

```rust
#[repr(u64)]
pub enum SlotState {
    Free      = 0,
    Reserved  = 1,
    Live      = 2,
    Dead      = 3, // SENTINEL_DEAD/no-upgrade barrier installed
    Retiring  = 4, // queued for EBR-safe reclamation
}
```

### 4.3 SlotWord Helpers

`SlotWord` is a non-atomic view of one loaded metadata word:

```rust
pub(crate) struct SlotWord(u64);

impl SlotWord {
    pub fn generation(self) -> u16;
    pub fn retain(self) -> u32;
    pub fn state(self) -> SlotState;

    pub fn with_generation(self, v: u16) -> Self;
    pub fn with_retain(self, v: u32) -> Self;
    pub fn with_state(self, s: SlotState) -> Self;

    pub fn inc_retain(self) -> Self;
    pub fn dec_retain(self) -> Self;
    pub fn inc_gen(self) -> Self;
}
```

Upgrade paths use one CAS over the whole word so generation, state, and retain
are checked atomically.

---

## 5. Slot Layout

Each slot is one contiguous block:

```text
slot start, aligned to max(align_of::<SlotMeta>(), align_of::<T>()):
+-------------------------------+
| SlotMeta                      |
+-------------------------------+
| alignment padding             |
+-------------------------------+
| T data                         |
+-------------------------------+
```

Generic retained-entity slots must not overwrite `T data` with retire-list
nodes before epoch quiescence. The EBR sidecar may live in metadata-reserved
storage, a bounded per-CPU retire-node pool, or another allocation-bounded
substrate area.

Pointer helpers:

```rust
pub(crate) unsafe fn meta_ptr_from_data<T>(data: *mut u8) -> *mut SlotMeta;
pub(crate) unsafe fn data_ptr_from_meta<T>(meta: *mut SlotMeta) -> *mut T;
```

---

## 6. Slabs, Kegs, And Zone Registry

The original slab mechanics remain implementation-valid:

- `Slab<T>` divides a page into slots and uses a bitmap to claim free slots.
- `Keg<T>` owns free/partial/full slab lists behind a spin lock.
- `Zone<T>` has per-CPU slot buckets for the hot path and batch-refills from
  `Keg<T>`.
- Empty slab pages may be released through `epoch::retire()` so stale guarded
  metadata reads cannot fault.
- A page-header back-pointer may be used to compute `slot_id` from a metadata
  pointer in O(1).

The registry shape also remains:

```rust
pub unsafe trait ZoneAllocated: Sized + 'static {
    fn zone() -> &'static Zone<Self>;
}
```

Each kernel entity module owns its static zone:

```rust
pub static PROCESS_IDENTITY_ZONE: Zone<ProcessIdentity> = Zone::const_new();

unsafe impl ZoneAllocated for ProcessIdentity {
    fn zone() -> &'static Zone<Self> { &PROCESS_IDENTITY_ZONE }
}
```

`zone_for::<T>(zone_id)` may debug-check the encoded zone id against `T::zone()`,
but release builds should monomorphize to the static zone for `T`.

---

## 7. Zone Reservation And Signing

### 7.1 `ZoneReservation<T>`

```rust
pub struct ZoneReservation<T: 'static> {
    zone_id: ZoneId,
    slot_id: u32,
    meta: *mut SlotMeta,
    data: *mut T,
    _not_send: PhantomData<*mut ()>,
}
```

Properties:

- `!Copy`, `!Send`, `!Sync`.
- Drop rollback returns the slot to Free.
- No generation increment on rollback.
- No `T::drop` on rollback because no value has been signed.

### 7.2 `zone::reserve`

```rust
pub fn reserve<T: ZoneAllocated>() -> Result<ZoneReservation<T>, AllocError>;
```

The reserve path:

1. Pop a slot id from the per-CPU bucket or refill from `Keg<T>`.
2. Verify metadata is `Free`, `retain == 0`.
3. Store `Reserved`.
4. Return `ZoneReservation<T>`.

### 7.3 `zone::sign`

```rust
pub fn sign<T: ZoneAllocated>(reservation: ZoneReservation<T>, value: T) -> Cap<T>;
```

`sign` is the zone publication primitive:

1. Write `value` into the reserved data area.
2. CAS `Reserved -> Live`, setting `retain = 1`.
3. Consume the reservation so rollback cannot run.
4. Return the first `Cap<T>`.

`sign` is infallible after successful reservation and value construction.

---

## 8. `Cap<T>`

`Cap<T>` is identity retention. It may be compactly encoded as
`zone_id + slot_id`; it does not need to store a generation because a live Cap
prevents slot reuse.

```rust
pub struct Cap<T: 'static> {
    raw: u32,
    _marker: PhantomData<Arc<T>>,
}
```

Construction paths are restricted:

1. `zone::sign`
2. `Cap::clone`
3. `IdentRef::to_cap` / `Weak::upgrade` through guarded observation

There is no public `from_raw`.

### 8.1 `Cap::clone`

`clone` CAS-increments `retain` only if state is `Live`. Seeing `Dead` or
`Retiring` is a bug for a valid Cap and should be debug-asserted.

### 8.2 `Cap::downgrade`

```rust
impl<T> Cap<T> {
    pub fn downgrade(&self) -> Weak<T> {
        let meta = unsafe { &*self.meta_ptr() };
        let cur = meta.load(Ordering::Acquire);
        debug_assert_eq!(cur.state(), SlotState::Live);
        Weak {
            raw: self.raw,
            generation: cur.generation(),
            _marker: PhantomData,
        }
    }
}
```

No counter changes. `Weak<T>` has no Drop obligations.

### 8.3 Final `Cap::drop`

```rust
impl<T: ZoneAllocated> Drop for Cap<T> {
    fn drop(&mut self) {
        let meta = unsafe { &*self.meta_ptr() };

        let old = loop {
            let cur = meta.load(Ordering::Acquire);
            debug_assert_eq!(cur.state(), SlotState::Live);
            debug_assert!(cur.retain() > 0);
            let new = cur.dec_retain();
            match meta.cas(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(old) => break old,
                Err(_) => continue,
            }
        };

        if old.retain() > 1 {
            return;
        }

        // Final identity retention: install no-upgrade barrier.
        loop {
            let cur = meta.load(Ordering::Acquire);
            let new = cur.with_state(SlotState::Dead);
            if meta.cas(cur, new, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                break;
            }
        }

        try_retire::<T>(self.data_ptr() as *mut u8);
    }
}
```

The destructor for `T` runs from the reclaim callback after epoch quiescence,
not synchronously on final `Cap::drop`, because existing `IdentRef<'g, T>`
values may still read identity fields.

---

## 9. `Weak<T>` And `IdentRef<'g, T>`

### 9.1 `Weak<T>`

```rust
pub struct Weak<T: 'static> {
    raw: u32,
    generation: u16,
    _marker: PhantomData<T>,
}
```

`Weak<T>` is a stale-tolerant, non-retaining slot handle. It is valid for
resolution-only bindings and caches, not for addressability obligations.

### 9.2 `Weak::observe`

```rust
impl<T: ZoneAllocated> Weak<T> {
    pub fn observe<'g>(&self, guard: &'g Guard) -> Option<IdentRef<'g, T>> {
        let zone = zone_for::<T>(self.zone_id());
        let (meta, data) = zone.slot_ptrs_if_present(self.slot_id(), guard)?;

        loop {
            let cur = unsafe { (*meta).load(Ordering::Acquire) };
            if cur.generation() != self.generation {
                return None;
            }
            if cur.state() != SlotState::Live {
                return None;
            }
            return Some(unsafe { IdentRef::from_raw(data, guard, self.generation) });
        }
    }
}
```

`slot_ptrs_if_present` must be safe under `Guard`: if the slab has been retired
and physically released, lookup returns `None`; if the slab is still present,
the guard prevents it from being freed while metadata is inspected.

### 9.3 `IdentRef::to_cap`

```rust
impl<'g, T: ZoneAllocated> IdentRef<'g, T> {
    pub fn to_cap(&self) -> Result<Cap<T>, Dead> {
        let meta = self.meta();
        loop {
            let cur = meta.load(Ordering::Acquire);
            if cur.generation() != self.generation {
                return Err(Dead);
            }
            if cur.state() != SlotState::Live {
                return Err(Dead);
            }
            if cur.retain() == RETAIN_SENTINEL_DEAD {
                return Err(Dead);
            }
            let new = cur.inc_retain();
            match meta.cas(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(Cap::new(self.zone_id(), self.slot_id())),
                Err(_) => continue,
            }
        }
    }
}
```

This is the bridge from EBR observation to refcounted retention.

---

## 10. Payload Evidence

For co-located entities:

```rust
impl Entity for AddressSpace {
    type OperationalEvidence = Cap<AddressSpace>;
}
```

For split entities:

```rust
struct ProcessIdentity {
    payload: PayloadBinding<ProcessPayload>,
}

impl Entity for ProcessIdentity {
    type OperationalEvidence = PayloadCap<ProcessPayload>;
}
```

For compound-payload entities:

```rust
enum RNodeOperationalEvidence {
    Open(OpenPin<RNode>),
    Link(LinkPin<RNode>),
    Synthetic(SyntheticProjectionPin<RNode>),
}
```

The zone module provides storage and retention CASes. The entity declares which
payload evidence type satisfies operational use.

---

## 11. Retire And Reclaim

```rust
fn try_retire<T: ZoneAllocated>(data_ptr: *mut u8) {
    let meta = unsafe { &*meta_ptr_from_data::<T>(data_ptr) };
    loop {
        let cur = meta.load(Ordering::Acquire);
        if cur.retain() != 0 || cur.state() != SlotState::Dead {
            return;
        }
        let new = cur.with_state(SlotState::Retiring);
        match meta.cas(cur, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                if unsafe { epoch::retire(data_ptr, reclaim_slot::<T>) }.is_ok() {
                    return;
                }
                epoch::try_drain(bounded_retry_budget);
                unsafe { epoch::retire(data_ptr, reclaim_slot::<T>) }
                    .expect("retire enqueue failed after bounded drain");
                return;
            }
            Err(_) => continue,
        }
    }
}
```

The five-state slot model intentionally has no "pending retire retry" state.
Once the CAS publishes `Retiring`, the slot must be handed to EBR. If the
retired-node pool is temporarily full, the implementation may perform one
bounded drain and retry. If enqueue still fails, the kernel fail-fasts: leaving
the slot outside both the allocator and the EBR queue would create an
unbounded leak or require a sixth lifecycle state, which this design rejects.

Reclaim callback:

```rust
unsafe fn reclaim_slot<T: ZoneAllocated>(data_ptr: *mut u8) {
    let meta_ptr = meta_ptr_from_data::<T>(data_ptr);
    let slot_id = slot_id_from_meta::<T>(meta_ptr);

    ptr::drop_in_place(data_ptr as *mut T);

    loop {
        let cur = (*meta_ptr).load(Ordering::Acquire);
        debug_assert_eq!(cur.state(), SlotState::Retiring);
        debug_assert_eq!(cur.retain(), 0);
        let new = cur.inc_gen().with_state(SlotState::Free);
        if (*meta_ptr).cas(cur, new, Ordering::Release, Ordering::Relaxed).is_ok() {
            break;
        }
    }

    T::zone().return_slot(slot_id);
}
```

Large destructors must be split through bounded-work reclaim items. The final
retention holder should not perform unbounded destruction on a hot path.

---

## 12. Lifecycle

```text
Free
  -> Reserved             zone::reserve
  -> Live                 zone::sign, retain=1, returns Cap<T>
  -> Dead                 final retention drop installs no-upgrade barrier
  -> Retiring             try_retire CAS; queued in EBR
  -> Free + generation++  reclaim callback after epoch quiescence
```

`Weak<T>` is outside this lifecycle. It may outlive any number of slot
lifecycles, but generation checks make it inert after reuse.

---

## 13. Binding Translation

Zone storage is driven by object-model obligations:

Every reclaimable upper entity is backed by a policy-based zone declaration,
but operation code sees only role-derived public types:

| Role | Public surface | Policy consequence |
|---|---|---|
| Cap-owned payload or identity | `Cap<T>`, `PayloadCap<T>` | retained/refcounted reclamation |
| Weak lookup hint | `Weak<T>` | no retention; generation-checked observation |
| Witness / identity-slot lookup | `IdentRef<'g, T>` inside a witness | EBR guard lifetime |
| Projection row | projection-specific reference or row type | EBR observation and revalidation |

| Upper-level declaration | Stored evidence |
|---|---|
| `Binding<T, ResolutionOnly>` | `Weak<T>` or `()` |
| `Binding<T, Addressability>` | `Cap<T>` |
| `Binding<T, Operational>` | `T::OperationalEvidence` |

Examples:

- PID namespace entries that promise zombie-stable addressability store
  retaining evidence such as `Cap<PidName>` / `Cap<ProcessIdentity>` according
  to the namespace design.
- Dcache hints may store `Weak<DEntry>`.
- bdev-fs's `devt -> Weak<PageContainer>` coherence map is a stale-tolerant
  cache, not ownership.
- TTY RNodes store `Cap<TtyIdentity>` because the RNode promises addressability
  to the same terminal identity.

---

## 14. Invariants

| ID | Invariant | Guaranteed by |
|---|---|---|
| ZONE-1 | `Cap<T>` can only be produced by `zone::sign`, `clone`, or guarded upgrade | constructors are private |
| ZONE-2 | `Weak<T>` contributes no retention | no weak counter and no Drop side effect |
| ZONE-3 | old `Weak<T>` cannot observe a reused slot | generation check under `Guard` |
| ZONE-4 | final retention installs no-upgrade barrier before retire | `Live -> Dead` CAS |
| ZONE-5 | `T::drop` runs only after epoch quiescence for EBR-observed slots | reclaim callback is EBR-delayed |
| ZONE-6 | reservation rollback does not consume generation | `ZoneReservation::drop` returns `Reserved -> Free` |
| ZONE-7 | `zone::sign` is infallible after reservation | no allocation after reserve; only write + CAS |
| ZONE-8 | upper subsystems use policy-based zones without selecting policy at call sites | `Zone<T>` hides policy; role-shaped evidence types express obligations |
| ZONE-9 | addressability bindings do not store non-retaining weak evidence | obligation derivation: addressability -> `Cap<T>` |
| ZONE-10 | static platform/device facts are not zone entities | use `&'static T`, never `Cap<T>` |
