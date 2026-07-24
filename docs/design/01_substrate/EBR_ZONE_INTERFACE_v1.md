# EBR / Zone Interface Adaptation — v1

<!-- txdoc:01-SUBSTRATE-EBR-ZONE-INTERFACE-V1 -->

**Status.** Draft design decision.

**Purpose.** Adapt the imported EBR and Zone/Cap implementation sketches
([`02_EBR_design.en-US.md`](../../ebr-zone/02_EBR_design.en-US.md),
[`03_Zone_Cap_object_storage_design.en-US.md`](../../ebr-zone/03_Zone_Cap_object_storage_design.en-US.md)) to
txKernel's current object-model interface. The imported sketches are useful
mechanics notes, but they speak an OSTD-local API (`pin`, `WeakCap`, weak-count
retention, process-block examples) rather than the architecture vocabulary:
`Guard`, `Weak<T>`, `IdentRef<'g, T>`, `Cap<T>`, `PayloadCap<T>`,
`OperationalEvidence`, `zone::reserve`, and `zone::sign`.

Companion documents:

- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) — reference
  hierarchy, obligations, semantic vs physical reclamation.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md)
  — substrate layout and zone reservation/signing surface.
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — guard-scoped
  witness and upgrade discipline.

---

## 1. Decision
<!-- txdoc:EBR-ZONE-DECISION-1 -->

Use **policy-based zones as the universal upper-subsystem lifetime substrate**,
but do **not** expose a policy-parameterized zone API to upper semantic
subsystems.

Upper subsystems should speak only the object-model interface:

```rust
epoch::guard() -> Guard

zone::reserve<T>(&Zone<T>) -> Result<ZoneReservation<T>, ZoneError>
zone::sign<T>(ZoneReservation<T>, T) -> Cap<T>

Weak<T>
IdentRef<'g, T>
Cap<T>
PayloadCap<T>
T::OperationalEvidence
```

The policy choice belongs to substrate implementation and entity-zone
declarations, not to operation code. A process step should not choose
`Zone<T, RcPolicy>` or `Zone<T, EbrPolicy>` at call sites. It should reserve a
typed zone slot, sign it, store the returned evidence according to the binding
obligation, and let the declared entity/reference family determine the
retention and retire mechanics.

The upper language stays role-shaped:

| Role named by subsystem prose | Public type family | Hidden policy family |
|---|---|---|
| Owned identity or payload | `Cap<T>`, `PayloadCap<T>` | retained/refcounted slot policy |
| Stale-tolerant lookup hint | `Weak<T>` | generation check, no retention |
| Witness / check result | witness carrying `IdentRef<'g, T>` | EBR guard scope |
| Identity table slot | `IdentitySlot<T>` / index entry storing `Cap<T>` | EBR-observed slot plus retained evidence |
| Projection row or read-only view | `ProjectionRef<'g, T>` / projection row | EBR observation plus revalidation |

Thus "policy-based zone" is the implementation substrate; `PayloadCap<T>`,
`Weak<T>`, `IdentitySlot<T>`, `WitnessSlot<T>`, and projection witnesses are
the architectural surface.

This keeps the upper layers aligned with `OBL-*`, `WIT-*`, and `BIF-*`:

- Checks produce `IdentRef<'g, T>` witnesses, never policy handles.
- Execution upgrades observations into `Cap<T>` or operational evidence.
- Structure stores published evidence matching binding obligations.
- Reclamation behavior is reviewable from entity factoring and evidence type.

---

## 2. Interface Vocabulary Mapping
<!-- txdoc:EBR-ZONE-INTERFACE-VOCABULARY-MAPPING-1 -->

| Imported draft term | txKernel interface term | Adaptation |
|---|---|---|
| `epoch::pin()` | `epoch::guard()` | Guard entry API should use object-model wording. CPU pinning may be an implementation detail inside `Guard`. |
| `EpochGuard` | `Guard` or `EpochGuard` re-exported as `Guard` | Public docs should use `Guard` unless a low-level epoch module needs the longer name. |
| `WeakCap<T>` | `Weak<T>` | Rename and change semantics: non-retaining generation handle. |
| `weak_count` | none for `Weak<T>` | Delete from semantic reclamation. Weak references do not block retirement. |
| `ReservedSlot<T>::init` | `zone::sign` | Signing consumes a reservation, writes the value, publishes the slot, and returns `Cap<T>`. |
| `ReservedSlot<T>` | `ZoneReservation<T>` | Linear reserve token with Drop rollback. |
| `Cap::get() -> &T` | `Cap<T>` deref / `Cap::ident_ref(&Guard)` | Direct deref is allowed only while a live `Cap` exists. Guarded observation should use `IdentRef`. |
| `Tombstone` | `SENTINEL_DEAD` / dead slot state | Use sentinel wording for no-upgrade barrier. |

---

## 3. Upper-Language Translation
<!-- txdoc:EBR-ZONE-UPPER-LANGUAGE-TRANSLATION-1 -->

Subsystem specs should describe object shape, binding obligations, and
operation needs. They should not describe allocator policies directly. The type
layer derives zone/reference types from those declarations.

```text
entity shape
  + binding obligation
  + operation need
      -> zone-backed type
      -> stored evidence type
      -> witness / upgrade API
```

### 3.1 Shape To Zone Type
<!-- txdoc:EBR-ZONE-UPPER-LANGUAGE-TRANSLATION-SHAPE-TO-ZONE-TYPE-1 -->

| Upper-level declaration | Zone-derived type shape |
|---|---|
| `entity T: CoLocated` | `Zone<T>`; identity and payload coincide. |
| `entity X: IdentityPayload { identity: XIdentity, payload: XPayload }` | `Zone<XIdentity>` plus `Zone<XPayload>`; identity stores a payload binding. |
| `entity T: IdentityOnly` | `Zone<T>`; no payload evidence type beyond identity retention. |
| `entity T: CompoundPayload` | `Zone<T>` plus typed operational contributors. |
| `type V: BindingValue` | No `Zone<V>`; stored by value inside an authoritative container. |
| `type S: StaticFact` | No zone and no refcount; use `&'static S` or a static table reference. |
| `type N: ObserverNode` | Hidden substrate storage; EBR-retired, not exposed as `Cap<N>` or `Weak<N>`. |

Examples:

```rust
// Co-located.
entity AddressSpace: CoLocated
// derives:
static ADDRESS_SPACE_ZONE: Zone<AddressSpace>;
type AddressSpaceCap = Cap<AddressSpace>;
type AddressSpaceWeak = Weak<AddressSpace>;
type AddressSpaceRef<'g> = IdentRef<'g, AddressSpace>;
type AddressSpaceOperationalEvidence = Cap<AddressSpace>;
```

```rust
// Identity/payload split.
entity Process: IdentityPayload {
    identity: ProcessIdentity,
    payload: ProcessPayload,
}
// derives:
static PROCESS_IDENTITY_ZONE: Zone<ProcessIdentity>;
static PROCESS_PAYLOAD_ZONE: Zone<ProcessPayload>;

type ProcessIdentityCap = Cap<ProcessIdentity>;
type ProcessPayloadEvidence = PayloadCap<ProcessPayload>;

struct ProcessIdentity {
    payload: PayloadBinding<ProcessPayload>,
}
```

```rust
// Binding value, not an entity.
type VmEntry: BindingValue
// derives no zone. It is stored in:
PersistentBTree<VAddrRange, VmEntry>
```

```rust
// Static platform/device fact, not an entity.
type CharDeviceBinding: StaticFact
// derives:
&'static CharDeviceBinding
// and explicitly does not derive:
Cap<CharDeviceBinding>
```

### 3.2 Obligation To Evidence
<!-- txdoc:EBR-ZONE-UPPER-LANGUAGE-TRANSLATION-OBLIGATION-TO-EVIDENCE-1 -->

Binding obligations derive stored evidence:

| Upper-level binding | Derived evidence |
|---|---|
| `Binding<T, ResolutionOnly>` | `Weak<T>` or `()` |
| `Binding<T, Addressability>` | `Cap<T>` |
| `Binding<T, Operational>` | `T::OperationalEvidence` |

Illustrations:

```rust
Binding<ProcessIdentity, Addressability>
// derives:
Cap<ProcessIdentity>
```

```rust
Binding<RNode, Operational>
// derives:
RNode::OperationalEvidence
// for example an OpenPin, LinkPin, or synthetic projection pin.
```

```rust
Binding<PageContainer, ResolutionOnly>
// derives:
Weak<PageContainer>
```

The obligation is part of the container's type. A namespace/index storing
addressability evidence must not silently downgrade to `Weak<T>` because that
would change the semantic promise from "same identity remains addressable" to
"maybe stale lookup hint."

### 3.3 Operation Need To Handle Type
<!-- txdoc:EBR-ZONE-UPPER-LANGUAGE-TRANSLATION-OPERATION-NEED-TO-HANDLE-TYPE-1 -->

Within a step, the handle type follows the operation's need:

| Operation need | Handle type |
|---|---|
| Stale-tolerant hint | `Weak<T>` |
| Current observation under a guard | `IdentRef<'g, T>` |
| Cross-step identity stability | `Cap<T>` |
| Payload-using operation | `T::OperationalEvidence` |
| Backend/static dispatch | `&'static T` or FS/backend trait object, not `Cap<T>` |

Pattern:

```rust
let guard = epoch::guard();
let witness = checks::require_x(..., &guard)?;      // carries IdentRef<'g, T>
let cap = witness.target.to_cap()?;                 // Cap<T>, if addressability needed
let op = T::upgrade_operational(&cap)?;             // T::OperationalEvidence, if payload needed
```

Cross-step continuation stores `cap` or `op`, never `witness` or
`IdentRef<'g, T>`.

### 3.4 Derived Type Families
<!-- txdoc:EBR-ZONE-UPPER-LANGUAGE-TRANSLATION-DERIVED-TYPE-FAMILIES-1 -->

The following families are generated conceptually for every zone-backed entity
type `T`:

```rust
Zone<T>
ZoneReservation<T>
Weak<T>
IdentRef<'g, T>
Cap<T>
T::OperationalEvidence
```

For split entities, `OperationalEvidence` is usually expressed on the identity:

```rust
impl Entity for MountIdentity {
    type OperationalEvidence = MountPayloadPin;
}
```

or:

```rust
impl Entity for ProcessIdentity {
    type OperationalEvidence = PayloadCap<ProcessPayload>;
}
```

The exact implementation spelling may be a `PayloadCap<Payload>` or a typed
contribution. The upper-level contract is "payload evidence reached through the
identity," not "a direct namespace binding to payload."

### 3.5 Translation Checklist
<!-- txdoc:EBR-ZONE-UPPER-LANGUAGE-TRANSLATION-TRANSLATION-CHECKLIST-1 -->

When writing or reviewing a subsystem spec:

1. If the prose says "object," classify it as co-located, split, identity-only,
   compound-payload, binding value, static fact, or observer node.
2. If the prose says "reference," name the promise: stale hint, observation,
   addressability, or operational use.
3. If a container stores a reference, declare the binding obligation and derive
   evidence from it.
4. If an operation crosses a step boundary, store `Cap<T>` or operational
   evidence, not a witness.
5. If a type is static board/platform state, do not invent a zone or `Cap<T>`.
6. If a type is just an authoritative container value, do not invent a zone for
   it unless it gains independent identity and reclamation.

Short form:

```text
shape declares zones
obligation declares stored evidence
checks produce witnesses
execution upgrades witnesses
structure stores evidence
```

---

## 4. Reference Semantics
<!-- txdoc:EBR-ZONE-REFERENCE-SEMANTICS-1 -->

The reference hierarchy remains:

```text
Weak<T>
    -> observe under Guard
IdentRef<'g, T>
    -> upgrade by SENTINEL_DEAD-guarded CAS
Cap<T>
    -> upgrade payload/contribution
T::OperationalEvidence
```

### 4.1 `Weak<T>` Is Non-Retaining
<!-- txdoc:EBR-ZONE-REFERENCE-SEMANTICS-WEAK-T-IS-NON-RETAINING-1 -->

`Weak<T>` stores a slot key and generation snapshot. It contributes no retention
and has no destructor obligations:

```rust
pub struct Weak<T> {
    key: ZoneKey,
    generation: Generation,
    _marker: PhantomData<T>,
}
```

Observing a weak reference requires a guard:

```rust
impl<T> Weak<T> {
    pub fn observe<'g>(&self, guard: &'g Guard) -> Option<IdentRef<'g, T>>;
}
```

`observe` performs:

1. Resolve `ZoneKey` through the zone directory while `guard` is held.
2. If the backing slab/slot is no longer present, return `None`.
3. Load slot metadata.
4. Check generation and live/dead state.
5. Produce `IdentRef<'g, T>` only if the slot still names the same live entity.

This differs from the imported design, where weak references increment
`weak_count` and keep slot reclamation waiting. That behavior conflicts with
`object_model_v2`: resolution-only evidence must not promise retention.

### 4.2 `IdentRef<'g, T>` Is EBR Observation
<!-- txdoc:EBR-ZONE-REFERENCE-SEMANTICS-IDENTREF-G-T-IS-EBR-OBSERVATION-1 -->

`IdentRef<'g, T>` is a guard-bound observation pointer. It contributes no
retention and cannot cross guard, step, thread, or async boundaries.

`IdentRef` may read stable identity fields while the slot is live. Once a slot
is marked dead, new `IdentRef` construction fails. Existing `IdentRef`s remain
memory-safe until the guard drops because physical reclamation and destructor
execution are deferred past epoch quiescence.

### 4.3 `Cap<T>` Is Identity Retention
<!-- txdoc:EBR-ZONE-REFERENCE-SEMANTICS-CAP-T-IS-IDENTITY-RETENTION-1 -->

`Cap<T>` is the only generic identity-retaining reference. Addressability
bindings store `Cap<Identity>`, not `Weak<T>`.

Upgrade from `IdentRef<'g, T>` uses one metadata CAS that checks:

- generation still matches;
- state is live;
- retention has not reached `SENTINEL_DEAD`;
- retention increment does not overflow.

On success it returns `Cap<T>`. On failure, the operation degrades to clean
failure (`ESTALE`, `ENOENT`, `ESRCH`, or subsystem-specific errno).

### 4.4 Operational Evidence Pins Payload
<!-- txdoc:EBR-ZONE-REFERENCE-SEMANTICS-OPERATIONAL-EVIDENCE-PINS-PAYLOAD-1 -->

`T::OperationalEvidence` is still entity-specific:

- co-located entity: `OperationalEvidence = Cap<T>`;
- split entity: payload evidence is `PayloadCap<Payload>` or a typed
  contribution reached through identity;
- compound payload: typed contributions such as `LinkPin`, `OpenPin`,
  `MapPin`, `CachePin`.

Policy does not change this obligation surface.

---

## 5. Zone Policies
<!-- txdoc:EBR-ZONE-ZONE-POLICIES-1 -->

Substrate may implement slots with policy families, but these policies are
selected by entity/reference declarations and hidden behind type aliases.

### 5.1 `RetainedEntitySlot`
<!-- txdoc:EBR-ZONE-ZONE-POLICIES-RETAINEDENTITYSLOT-1 -->

For semantic entities that can be named by `Cap<T>`.

Used by:

- co-located entities: `DEntry`, `FdEntry`, `OpenFile`;
- identity halves: `ProcessIdentity`, `ThreadIdentity`, `MountIdentity`,
  `SocketIdentity`, `TtyIdentity`;
- payload halves when represented as payload-zone objects.

Metadata:

```text
generation
state: Free | Reserved | Live | Dead | Retired
retain: identity-retention count or SENTINEL_DEAD
```

No `weak_count`. `Weak<T>` is a stale-tolerant handle, not a lifecycle
contributor.

Lifecycle:

```text
Free
  -> Reserved             zone::reserve
  -> Live                 zone::sign, returns first Cap<T>
  -> Dead                 last identity retention wins SENTINEL_DEAD CAS
  -> Retired              enqueue into bounded reclaim queue
  -> Free + generation++  after epoch quiescence and destructor
```

The last retention holder does **not** run `T::drop` immediately if guarded
readers may still hold `IdentRef<'g, T>`. It marks the slot dead and enqueues
retirement. The reclaim queue runs the destructor only after the epoch-safe
window. Large destructors split their work under bounded-work reclamation.

Retire enqueue failure is fail-fast in the five-state design. After
`Dead -> Retired/Retiring` is claimed, there is no sixth state where the slot
can safely remain discoverable for a later retry: it is non-upgradeable and not
allocator-free, but it is also not yet owned by the EBR queue. Therefore the
zone implementation may run one bounded `epoch::try_drain`/retry to relieve
transient retired-node pool pressure. If the second enqueue still fails, this is
a substrate capacity invariant violation and the kernel panics rather than
silently leaking or reusing the slot.

### 5.2 `PayloadSlot`
<!-- txdoc:EBR-ZONE-ZONE-POLICIES-PAYLOADSLOT-1 -->

For independently reclaimable payload boxes. This is still retained, but the
retention predicate is payload-specific rather than identity-specific.

Used by:

- `ProcessPayload`;
- `ThreadPayload`;
- `MountPayload`;
- `SocketPayload`;
- other split payloads.

The identity owns an atomic payload field:

```rust
payload: AtomicPayload<Option<PayloadCap<Payload>>>
```

Payload teardown is a semantic commit on the identity's payload field. After
withdrawal, new operational upgrades fail; existing payload evidence keeps the
payload alive until dropped; final payload retirement is EBR-delayed.

### 5.3 `ObserverNodeSlot`
<!-- txdoc:EBR-ZONE-ZONE-POLICIES-OBSERVERNODESLOT-1 -->

For substrate nodes that are traversed under EBR but have no public `Cap<T>`:

- index nodes;
- intrusive-list materialization nodes;
- transient container nodes whose semantic lifetime is owned by a container
  mutation primitive.

These are EBR-retired but not exposed as object-model entities. Upper
subsystems should not store `Weak<T>` or `Cap<T>` to them. They are substrate
implementation details.

### 5.4 No Upper-Layer `Zone<T, Policy>` Surface
<!-- txdoc:EBR-ZONE-ZONE-POLICIES-NO-UPPER-LAYER-ZONE-T-POLICY-SURFACE-1 -->

Avoid this in semantic code:

```rust
Zone<ProcessIdentity, RcPolicy>
Zone<ProcessIdentity, EbrPolicy>
```

Prefer:

```rust
static PROCESS_IDENTITY_ZONE: Zone<ProcessIdentity>;
static PROCESS_PAYLOAD_ZONE: Zone<ProcessPayload>;
```

The zone declaration can choose the hidden substrate policy once. Operation code
sees the same reserve/sign/reference API for every semantic entity.

---

## 6. Binding Consequences
<!-- txdoc:EBR-ZONE-BINDING-CONSEQUENCES-1 -->

Obligation evidence remains the source of truth:

| Obligation | Stored evidence | Reclamation effect |
|---|---|---|
| Resolution-only | `Weak<T>` or `()` | No retention. May become stale. |
| Addressability | `Cap<T>` | Keeps identity structurally live. |
| Operational | `T::OperationalEvidence` | Keeps payload/contribution live and entails identity retention. |

Examples:

- PID namespace numeric entries that promise zombie-stable addressability store
  `Cap<PidName>` or `Cap<ProcessIdentity>` depending on the resolved object
  design, not `Weak<ProcessIdentity>`.
- A dcache hint may store `Weak<DEntry>` because it is resolution-only and must
  tolerate stale entries.
- Parent `children` entries store `Cap<ProcessIdentity>` through the zombie
  window. `waitpid` withdrawal drops that `Cap`, enabling identity retirement.
- A signal-fd target that tolerates target death stores `Weak<ProcessIdentity>`;
  read/poll paths re-observe and return target-gone behavior if upgrade fails.

This removes the imported design's `weak_count` split-lifetime pattern. The
upper subsystem chooses a binding obligation; the evidence type does the rest.

---

## 7. Adapted EBR Surface
<!-- txdoc:EBR-ZONE-ADAPTED-EBR-SURFACE-1 -->

The epoch module owns traversal safety and physical reclamation delay:

```rust
pub fn guard() -> Guard;

pub(crate) unsafe fn retire(
    ptr: NonNull<()>,
    reclaim: unsafe fn(NonNull<()>),
);

pub fn try_drain(budget: DrainBudget);
```

Implementation may still use:

- a global monotonically increasing epoch;
- per-CPU local epoch state;
- per-CPU retired lists;
- timer-triggered and pressure-triggered drains;
- CPU pinning inside `Guard`;
- a two-epoch safe margin.

The public architecture should not require callers to know these mechanics.
Callers hold `Guard`; zones call `retire`; `retire` records the current epoch
internally; the timer/allocator invokes `try_drain`.

---

## 8. Adapted Zone Surface
<!-- txdoc:EBR-ZONE-ADAPTED-ZONE-SURFACE-1 -->

The substrate zone API remains:

```rust
pub struct Zone<T> { /* policy hidden */ }
pub struct ZoneReservation<T> { /* linear */ }

pub fn reserve<T>(zone: &Zone<T>) -> Result<ZoneReservation<T>, ZoneError>;
pub fn sign<T>(reservation: ZoneReservation<T>, value: T) -> Cap<T>;
```

Reference operations:

```rust
impl<T> Cap<T> {
    pub fn downgrade(&self) -> Weak<T>;
    pub fn ident_ref<'g>(&self, guard: &'g Guard) -> IdentRef<'g, T>;
}

impl<T> Weak<T> {
    pub fn observe<'g>(&self, guard: &'g Guard) -> Option<IdentRef<'g, T>>;
    pub fn upgrade(&self, guard: &Guard) -> Option<Cap<T>> {
        self.observe(guard)?.to_cap().ok()
    }
}

impl<'g, T> IdentRef<'g, T> {
    pub fn to_cap(&self) -> Result<Cap<T>, Dead>;
}
```

`Weak::upgrade` is convenience; architecturally the bridge is still
`Weak -> IdentRef -> Cap`.

---

## 9. Static Zone Registration
<!-- txdoc:EBR-ZONE-STATIC-ZONE-REGISTRATION-1 -->

Static zone discovery is explicit. The kernel does not rely on linker-section
collection for zone registration in the current architecture line.

Each subsystem that declares static zones exposes one local registration hook:

```rust
pub(crate) fn register_zones() -> Result<(), ZoneError>;
```

The subsystem aggregate owns the single boot-time list:

```rust
pub fn register_all() -> Result<(), ZoneError> {
    process::register_zones()?;
    thread::register_zones()?;
    vm::register_zones()?;
    page_backed::register_zones()?;
    mount::register_zones()?;
    vfs::register_zones()?;
    tty::register_zones()?;
    Ok(())
}
```

`CoreInit` calls this aggregate exactly once on the BSP after
`tx_substrate::init::<P>()` has initialized epoch/zone runtime state and before
any AP is started, any zone reservation is attempted by semantic subsystems, or
any long-lived subsystem object is published. Registration is idempotent for the
same static `Zone<T>` so tests and smoke paths may call the aggregate again,
but production boot treats the aggregate as a fixed manifest rather than as
dynamic discovery.

AP initialization never registers zones. `tx_substrate::init_on_ap(cpu)` only
initializes per-CPU epoch/zone local state for zones already registered by the
BSP manifest. Adding a new static zone requires adding its type to its
subsystem's `register_zones()` hook and, if the subsystem is new, adding that
hook to the aggregate `register_all()` list.

Late registration is not part of normal semantic operation. A zone that has not
been registered by the BSP manifest is unavailable and `reserve_for<T>()` fails
with `ZoneError::NotRegistered`. This keeps compact zone IDs stable, gives AP
bucket initialization a closed set to prepare, and makes missing zone ownership
visible during boot or tests instead of silently depending on first use.

The explicit manifest is deliberately chosen over linker-section collection:

| Strategy | Decision | Reason |
|---|---|---|
| explicit `register_all()` | accepted | reviewable boot order, no linker-script dependency, deterministic tests |
| macro-assisted per-subsystem hook | allowed later | may reduce boilerplate while still feeding the explicit aggregate |
| linker-section auto-registration | rejected for now | hides ownership/order and creates board/linker coupling before it is needed |

---

## 10. What To Import From The Drafts
<!-- txdoc:EBR-ZONE-WHAT-TO-IMPORT-FROM-THE-DRAFTS-1 -->

Keep:

- per-CPU epoch state and bounded retired-list draining;
- packed metadata for generation/state/retention CAS;
- generation-tagged weak handles for ABA prevention;
- linear zone reservations with Drop rollback;
- infallible signing after successful reservation;
- slab/page delayed release through EBR;
- per-CPU slot caches as an implementation optimization.

Change:

- `pin()` to `guard()`;
- `WeakCap<T>` to non-retaining `Weak<T>`;
- remove `weak_count` from semantic reclamation;
- replace `strong == 0 && weak == 0 && Tombstone` with
  `retain == 0 -> SENTINEL_DEAD -> retire`;
- run destructors after epoch quiescence for EBR-observed slot contents;
- express split lifetimes with identity/payload zones and obligation evidence,
  not weak retention.

Reject:

- upper subsystem call sites selecting zone policies;
- namespace/addressability indexes storing non-retaining weak evidence when the
  obligation promises identity stability;
- immediate `T::drop` before guarded readers have quiesced;
- out-of-band `reclaim_permitted` flags.

### 10.1 Relationship To Owner Lanes And RCU Publication

<!-- txdoc:EBR-ZONE-OWNER-LANES-RCU-1 -->

[`OBJECT_API_LANES_v1.md`](../00_meta-framework/OBJECT_API_LANES_v1.md)
defines the semantic owner/root boundary above this interface. Zone and RCU
publication remain orthogonal beneath that boundary:

- zone gives semantic entities stable identity and role-shaped evidence;
- authoritative containers store `BindingValue` entries by value, including
  obligation-derived `Weak`, `Cap`, or operational evidence;
- `Published<T>` may replace immutable container roots and retire old roots
  through EBR without giving those roots semantic identity;
- tree/index nodes are private `ObserverNode` storage and never produce public
  `Cap<Node>` or `Weak<Node>`;
- old published snapshots may delay evidence Drop through the grace period,
  but fresh readers resolve only through the newly published root.

Moving a container to RCU does not require moving its nodes into zone storage.
Observer-node zones are an optional private allocation/reclamation strategy,
not an upper API or an RCU prerequisite.

---

## 11. Review Checklist
<!-- txdoc:EBR-ZONE-REVIEW-CHECKLIST-1 -->

When introducing a new zone-backed type:

1. Declare entity factoring: co-located, identity/payload, or compound payload.
2. Declare binding obligations for every published container slot.
3. Ensure addressability bindings store `Cap<Identity>`.
4. Ensure resolution-only caches store `Weak<T>` or no evidence.
5. Ensure operational paths upgrade to `T::OperationalEvidence`.
6. Ensure witnesses contain `IdentRef<'g, T>`, not `Cap<T>`.
7. Ensure no witness or guard crosses a step, await, or thread boundary.
8. Ensure destructor work is compatible with EBR-delayed reclamation and
   bounded-work draining.
9. Add every static `Zone<T>` to its subsystem `register_zones()` hook and to
   the aggregate `register_all()` manifest before any reservation path can use
   it.

---

## 12. Short Form
<!-- txdoc:EBR-ZONE-SHORT-FORM-1 -->

Policy-based zone is a substrate implementation technique, not an upper
subsystem programming model. Upper code uses obligations and evidence. The
zone maps those evidence types to hidden retention/EBR policies.

`Weak<T>` is not `WeakCap<T>`. It is a generation-tagged, non-retaining,
stale-tolerant handle. If a structure must keep an identity alive, it stores
`Cap<T>`.
