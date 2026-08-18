# Portable network Phase 0 implementation-readiness audit

Date: 2026-08-04

Scope: RV64 QEMU, LA64 QEMU, VisionFive 2, and the deferred LA real-board hook

Audit result: **ready: yes — ready to begin Phase 1/Phase 2 implementation**

This result means the active architecture now names enough ownership, public
types, lifetimes, binding evidence, init steps, failures, and dependencies to
write the portability harness and common resource model. It does **not** mean
the current Rust implementation already conforms or that DWMAC/Git works on
VisionFive 2.

## Canonical implementation contract

- [`NET_DEVICE_v1.md`](../../design/06_devices/NET_DEVICE_v1.md) is the owning
  resource/binder/network specification.
- [`HAL_v1.md`](../../design/01_substrate/HAL_v1.md) owns static platform
  selection, the immutable seed, controller IRQ mechanics, and domain-scoped
  DMA/cache methods.
- [`PAGE_SUBSTRATE_v1.md`](../../design/01_substrate/PAGE_SUBSTRATE_v1.md) owns
  constrained physical frame reservation and DMA frame/pin lifetime.
- [`DEVICE.md`](../../design/06_devices/DEVICE.md) owns tier-2 boot binding and
  class registries.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md)
  `txdoc:INV-V5-DEVRES` is the enforceable rule catalog. The v4 invariant file
  is retained only for carried-forward anchors and compatibility wording.

## Seven-axis audit

| Axis | Decision/evidence | Result |
|---|---|---|
| owner | HAL owns seed/controller/DMA mechanism; device owns final graph/binder/static registrations; substrate owns frames; net namespace/FIB owns address/route/neighbor; userspace owns DNS/proxy/DHCP | ready |
| public types | exact `DeviceId`, provenance, graph/resource, domain/constraint, provider/driver bundle, binder/report/error, IRQ route/context/table, registration/projection/selector types are named in NET_DEVICE §§2–7 | ready |
| lifetimes | seed/final graph/bound registrations are `&'static`; builder/reservation is private and linear; DMA run/pins/mapping are retained by concrete driver state; namespace projection has namespace lifetime | ready |
| binding/evidence | compatible/bus match chooses one highest-priority descriptor; typed decoder requires role-specific resources; `BoundDeviceKey` + `DeviceId` link IRQ and registration; `DmaMapping` + pins link device access to retained RAM | ready |
| steps/publication | Phase 3A freezes graph; Phase 3B is `OneShotStepOp` with observe, check, reserve, commit, publish; sources remain masked until registries/table are visible | ready |
| failures | graph corruption/global capacity aborts with no publication; unsupported and candidate-local probe failure are reported without hiding later devices; ambiguity, missing/duplicate resource, IRQ conflict, DMA constraints/mapping, selection absence/ambiguity are typed | ready |
| dependencies/extensions | board binary selects `P` and `StaticDeviceBundle<P>`; substrate mapping precedes providers; freeze precedes binder; binder precedes devfs/userspace; synthetic provider exercises the future LA seam without a fictional board | ready |

## Frozen public shape

The implementation may refine private storage, but changing any row below is an
architecture change and must update the owning spec before code:

| Home | Public shape |
|---|---|
| `tx-hal` | `PlatformInfo::device_resources: &'static DeviceResourceGraph` |
| `tx-hal` | `DeviceId { provider, local }`, with firmware-path, complete PCI function, or stable platform-key local identity |
| `tx-hal` | `PlatformDevice { id, status, matches, resources, origin }` |
| `tx-hal` | closed `DeviceResource` variants and role-named MMIO/IRQ/DMA/dependency/MAC facts |
| `tx-hal` | `DmaDomain`, `DmaConstraints`, `DmaMapping`, domain-scoped `DmaIf` |
| device structure | owned `DeviceGraphBuilder` → one `&'static DeviceResourceGraph` freeze |
| board composition | `StaticDeviceBundle<P>` returning concrete resource-provider and driver descriptor slices |
| device execution | `DeviceBindReservation<P>`, `BoundDeviceKey`, `BoundDevice`, `DeviceBindReport` |
| kernel/device IRQ | `IrqRoute`, `DeviceIrqContext { route, bound }`, bucketed immutable dispatch table |
| network device | `NetDeviceRegistration { device_id, devt, ops }` without hardware identity by interface name |
| net namespace | `NetInterfaceProjection { ifindex, name, device }` and authoritative address/FIB/neighbor mutations |
| boot network | explicit `BootNetSelector` plus unique-firmware/only-candidate precedence and typed absence/ambiguity |

## Initialization and transaction boundary

```text
compile/link:
  choose ActivePlatform P + ActiveDeviceBundle D
      |
H2: P publishes immutable platform seed
      |
substrate: maps seed MMIO; frame allocator/heap become live
      |
Phase 3A: D resource providers enumerate once -> validate -> freeze final graph
      |
Phase 3B reserve:
  match -> typed decode -> driver state -> constrained DMA -> IRQ/registry slots
      |
Phase 3B commit/publish:
  initialize and ACK while masked
  -> atomically publish bound/class registries + kernel IRQ table
  -> arm device/unmask committed routes
      |
namespace projection -> authorized static/DHCP/userspace configuration
```

No fallible allocation or device probe starts after the global publication
boundary. Publication failures therefore cannot leave a half-visible registry.

## Failure classification

| Class | Required behavior |
|---|---|
| malformed seed/final graph | fatal boot-device transaction; publish nothing |
| unsupported/disabled device | report/skip; not a synthetic failure and not a fallback trigger |
| equal highest driver match | candidate failure `AmbiguousDriver` |
| missing/duplicate/incompatible typed role | candidate failure; continue with later records |
| driver probe failure | candidate failure; clean reservation rollback; continue |
| no DMA run/mapping satisfying constraints | typed DMA candidate failure; never truncate |
| duplicate/exclusive IRQ conflict | global table reservation failure; no publication |
| class registry overflow/duplicate identity/devt | global transaction failure; no publication |
| zero healthy NICs | valid empty registry; explicit boot-network request returns `NoOperationalDevice` |
| multiple healthy NICs without selector | registrations remain; boot configuration returns `Ambiguous` |
| unknown future LA board | no target/profile/success claim; only generic seam tests exist |

## Resolved blockers from the earlier audit

- The flat `DeviceInfo { kind, mmio, irq }` target is replaced by the exact
  graph/resource catalog; the old type is migration-only.
- Tier 2 now means static lifetime after one-shot boot binding, not compile-time
  resource values. Runtime hotplug remains tier 3.
- `NET_IRQ`/`net_irq()` is explicitly migration-only; final routes retain
  `DeviceId`, role, and `BoundDeviceKey`.
- DMA mapping/cache operations now consume a domain/mapping, while page
  substrate has a constrained run API and typed failure.
- The binder has an exact static composition seam (`StaticDeviceBundle<P>`),
  five-stage order, rollback owner, atomic publication rule, and error catalog.
- Network hardware identity, namespace projection, selection precedence, and
  configuration ownership are separated.
- The future LA seam has exact platform/bundle/host-profile responsibilities
  and explicit absent/unsupported behavior without guessed hardware values.
- New enforceable rules land in canonical v5 `DEVRES-*`; v4 contains only a
  compatibility correction.

## Nonblocking implementation work

These items are required by later gates but do not block beginning code from
the contract:

- implement `cargo xtask lint net-portability` and its positive/negative cases;
- implement the versioned scenario renderer and mutation fixtures;
- migrate current HAL/device/IRQ/DMA APIs and both QEMU targets;
- implement DWMAC/StarFive/PHY code and run real VF2 acceptance;
- identify the actual LA board and add its concrete platform/bundle/profile;
- retain the existing deferred tier-3 and advanced multi-uplink scope.

## Implementation entry decision

Begin with Phase 1, because the portability lint and mutation fixtures must
fail against the legacy code before Phase 2 removes it. Then implement the
common graph/binder types in dependency order: `tx-hal` facts → substrate DMA
reservation → kernel/device IRQ contexts → graph freeze/binder/registries.
