# Network Devices and Boot Network Configuration

<!-- txdoc:06-DEVICES-NET-DEVICE-V1 -->

**Status.** v1 implementation contract (2026-08-04).

**Purpose.** Define the portable boot-time resource, binding, interrupt, DMA,
network-device registration, interface projection, and boot-network ownership
model used by RV64 QEMU, LA64 QEMU, VisionFive 2, and the deferred LA real-board
extension seam. This document does not specify a particular NIC register model.

**Scope boundary.** The selected `P: TxPlatform`, the linked resource-provider
set, and the linked driver set are fixed at compile/link time. Resource discovery
and binding run once during boot. Successful bindings live for the kernel
lifetime. Runtime hotplug, detach, driver unloading, a runtime HAL manager, and
zone-backed dynamic devices remain tier-3 work.

**Companion documents.**

- [`HAL_v1.md`](../01_substrate/HAL_v1.md) owns platform selection, immutable
  platform resource seeds, interrupt-controller mechanics, and DMA/cache
  translation.
- [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md) owns physical
  frame reservations and constrained contiguous allocation.
- [`DEVICE.md`](DEVICE.md) owns the tier model, the one-shot binder, and static
  class registries.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) owns the enforceable
  `DEVRES-*` rules.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md)
  owns the five-stage observe/upgrade/reserve/commit/publish discipline.

---

## 1. Ownership and non-ownership

<!-- txdoc:NET-DEVICE-OWNERSHIP-1 -->

| Fact or object | Authoritative owner | Lifetime |
|---|---|---|
| selected platform implementation | board binary and concrete platform crate | compile/link time |
| platform resource seed | HAL `PlatformInfo` | `&'static`, immutable after H2 |
| final boot resource graph | device structure/index | `&'static`, frozen once before binding |
| static resource-provider and driver descriptors | linked device/driver crates | compile/link time |
| bound device and hardware identity | device subsystem | `&'static`, never detached in tier 2 |
| physical DMA frames and pins | page substrate | typed owned tokens |
| device-visible DMA mapping and cache transitions | selected platform `DmaIf` | owned by the bound driver |
| NIC registration | network device registry | `&'static`, published once |
| interface name and ifindex | network namespace projection | namespace lifetime |
| addresses, prefixes, routes, and neighbors | network namespace/FIB | until an authorized control-plane mutation |
| DNS and proxy configuration | userspace | userspace configuration lifetime |
| DHCP protocol state | userspace DHCP client | process/lease lifetime |

HAL resource records and tier-2 bindings are static facts, not zone entities.
They must not be wrapped in `Cap<T>`, `PayloadCap<T>`, or `Weak<T>`. Namespace,
socket, and route entities retain their existing role-shaped evidence.

### Zone-derived type policy

<!-- txdoc:NET-DEVICE-ZONE-DERIVED-TYPE-POLICY-1 -->

| Declaration | Public handle | Reclamation role |
|---|---|---|
| platform/final resource record | `&'static PlatformDevice` | immutable static fact; no cap |
| bound tier-2 device | `&'static BoundDevice` | static registration; no cap |
| network device registration | `&'static NetDeviceRegistration` | static registration; no cap |
| interface projection | namespace-owned interface identity/evidence | namespace entity; existing net policy applies |
| DMA frame owner | `OwnedFrameRun<'static, _>` plus `DmaPin<'static, _>` | substrate linear ownership |
| deferred hotplug device | future identity/payload pair | tier-3 zone entity; out of scope |

---

## 2. Immutable resource records

<!-- txdoc:NET-DEVICE-RESOURCE-GRAPH-1 -->

The public vocabulary below lives in `tx-hal`. It is semantic-free hardware
fact data and is usable before the heap. Slice elements and strings published
by a platform point into boot-owned static storage.

```rust
pub struct PlatformInfo {
    pub board: &'static str,                 // diagnostic only
    pub device_resources: &'static DeviceResourceGraph,
    pub timebase_frequency_hz: u64,
    pub possible_cpu_count: usize,
}

pub struct DeviceResourceGraph {
    /// HAL-owned mappings needed independently of a tier-2 driver, for example
    /// an interrupt controller or early console window.
    pub platform_mmio: &'static [MmioResource],
    /// Firmware/static devices known when this graph was frozen.
    pub devices: &'static [PlatformDevice],
    /// Shared DMA translation/coherency domains referenced by devices.
    pub dma_domains: &'static [DmaDomain],
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResourceProviderId(pub &'static str);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PciFunctionId {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeviceLocalId {
    FirmwarePath(&'static str),
    PciFunction(PciFunctionId),
    PlatformKey(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeviceId {
    pub provider: ResourceProviderId,
    pub local: DeviceLocalId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceMatchId {
    FirmwareCompatible(&'static str),
    Pci(PciDeviceMatch),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciDeviceMatch {
    pub vendor: u16,
    pub device: u16,
    pub subsystem_vendor: Option<u16>,
    pub subsystem_device: Option<u16>,
    pub class: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceStatus {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformDevice {
    pub id: DeviceId,
    pub status: DeviceStatus,
    pub matches: &'static [DeviceMatchId],
    pub resources: &'static [DeviceResource],
    pub origin: ResourceOrigin,
}
```

`DeviceId` is stable under device-list reordering, resource-list reordering,
interface renaming, and unrelated device insertion. Providers must derive it
from firmware path, complete PCI segment/BDF identity, or a stable
platform-local key. Discovery ordinal, generated MMIO name, physical address,
and namespace name are forbidden identity sources.

`ResourceProviderId` and `ResourceOrigin` are provenance and diagnostics. They
must not be used to choose a driver or inject platform policy into generic code.
`DmaDomainId::local` follows the same stability rule as `DeviceId`: it is a
provider-defined stable key (for example a firmware handle), never the domain's
position in an output slice.

### 2.1 Typed resources

<!-- txdoc:NET-DEVICE-TYPED-RESOURCES-1 -->

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResourceRole {
    /// A specification-defined `*-names` entry.
    Named(&'static str),
    /// A specification-defined ordinal when that binding has no names.
    Index(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceOriginKind {
    Firmware,
    BusEnumeration,
    PlatformStatic,
    CapabilityRefinement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceOrigin {
    pub provider: ResourceProviderId,
    pub record: &'static str,
    pub kind: ResourceOriginKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioResource {
    pub role: ResourceRole,
    pub phys: PhysRange,
    pub virt: VirtRange,
    pub flags: MmioFlags,
    pub origin: ResourceOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqTrigger { Edge, Level }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqPolarity { High, Low }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqSharing { Exclusive, Shared }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqResource {
    pub role: ResourceRole,
    pub line: u32,
    pub trigger: IrqTrigger,
    pub polarity: IrqPolarity,
    pub sharing: IrqSharing,
    pub origin: ResourceOrigin,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DmaDomainId {
    pub provider: ResourceProviderId,
    pub local: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderSpecifier {
    pub cells: &'static [u32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockRef { pub role: ResourceRole, pub provider: DeviceId, pub spec: ProviderSpecifier }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResetRef { pub role: ResourceRole, pub provider: DeviceId, pub spec: ProviderSpecifier }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SysconRef { pub role: ResourceRole, pub provider: DeviceId, pub spec: ProviderSpecifier }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MdioBusRef { pub role: ResourceRole, pub provider: DeviceId }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhyRef { pub role: ResourceRole, pub provider: DeviceId }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmemCellRef {
    pub provider: DeviceId,
    pub offset: u32,
    pub length: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacAddressSource {
    Firmware([u8; 6]),
    Nvmem(NvmemCellRef),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaDomainRef {
    pub role: ResourceRole,
    pub domain: DmaDomainId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceResource {
    Mmio(MmioResource),
    Irq(IrqResource),
    DmaDomain(DmaDomainRef),
    Clock(ClockRef),
    Reset(ResetRef),
    Syscon(SysconRef),
    MdioBus(MdioBusRef),
    Phy(PhyRef),
    MacAddress { role: ResourceRole, source: MacAddressSource },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    Mmio,
    Irq,
    DmaDomain,
    Clock,
    Reset,
    Syscon,
    MdioBus,
    Phy,
    MacAddress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceCapacityKind {
    Device,
    Resource,
    DmaDomain,
    IrqRoute,
}
```

This closed typed catalog is not an arbitrary firmware property bag.
Driver-specific decoders may match only specification-defined compatible/bus
IDs and resource roles. `ResourceRole::Index` is legal only where the external
binding defines that ordinal. A missing name never permits fallback to a
familiar address or another resource's ordinal.

### 2.2 DMA domains and constraints

<!-- txdoc:NET-DEVICE-DMA-DOMAIN-1 -->

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaCoherency { Coherent, NonCoherent }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaTranslation {
    Direct { offset: i64 },
    Managed { address_space: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaConstraints {
    pub dma_address_bits: u8,
    pub min_alignment: usize,
    pub segment_boundary: Option<u64>,
    pub max_segment_len: usize,
    pub max_segments: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaDomain {
    pub id: DmaDomainId,
    pub translation: DmaTranslation,
    pub constraints: DmaConstraints,
    pub coherency: DmaCoherency,
    pub origin: ResourceOrigin,
}
```

`segment_boundary` is a power-of-two window size, not a bit mask. No emitted
DMA segment may cross a window of that size; `None` adds no window constraint.
The smaller of two present window sizes is the stricter value.

The platform default `PlatformConfig::DMA_COHERENT` is only a compatibility
default for domains that firmware cannot describe. The effective fact consumed
by a driver is `DmaDomain::coherency`. Hardware capability registers may refine
constraints after reset by taking their strict intersection; they may never
widen a firmware/platform constraint.

### 2.3 Graph construction and freeze

<!-- txdoc:NET-DEVICE-GRAPH-FREEZE-1 -->

There are two immutable graph views because some buses cannot be enumerated
until substrate has mapped their controller windows:

1. `PlatformInfo::device_resources` is the platform seed, published before
   substrate. It contains platform mappings, firmware-described devices, DMA
   domains, and bus-controller resources.
2. After substrate mapping, the device subsystem runs the linked static
   `ResourceProviderDescriptor` set exactly once. Providers may append
   enumerated records to a private `DeviceGraphBuilder`.
3. `DeviceGraphBuilder::freeze()` validates and publishes one final
   `&'static DeviceResourceGraph`. Driver matching consumes only this final
   graph.

```rust
pub struct ResourceProviderDescriptor<P: TxPlatform> {
    pub id: ResourceProviderId,
    pub enumerate: fn(
        seed: &'static DeviceResourceGraph,
        out: &mut DeviceGraphBuilder,
    ) -> Result<(), ResourceGraphError>,
    pub _platform: PhantomData<fn() -> P>,
}

pub struct DeviceGraphBuilder { /* private boot-only storage */ }

pub struct DeviceRecordBuilder {
    pub id: DeviceId,
    pub status: DeviceStatus,
    pub matches: Vec<DeviceMatchId>,
    pub resources: Vec<DeviceResource>,
    pub origin: ResourceOrigin,
}

impl DeviceGraphBuilder {
    pub fn from_seed(seed: &'static DeviceResourceGraph)
        -> Result<Self, ResourceGraphError>;
    pub fn push_device(&mut self, device: DeviceRecordBuilder)
        -> Result<(), ResourceGraphError>;
    pub fn push_dma_domain(&mut self, domain: DmaDomain)
        -> Result<(), ResourceGraphError>;
    pub fn freeze(self)
        -> Result<&'static DeviceResourceGraph, ResourceGraphError>;
}
```

The builder owns all appended vectors until validation succeeds. `freeze`
moves them into one boot-lifetime arena, fixes the public slices, and publishes
the only final `&'static` graph. A failed builder drops its owned storage and
publishes nothing; providers never pre-leak individual candidate records.

The descriptor set is supplied by the compile-time `StaticDeviceBundle<P>` in
§3, not registered at runtime. A firmware-complete target may provide an empty
enumerator slice and freeze the seed unchanged. A PCI target selects a bundle
containing a one-shot PCI provider; this does not make PCI hotplug a tier-2
feature.

Exact graph failures are:

```rust
pub enum ResourceGraphError {
    DuplicateDeviceId(DeviceId),
    DuplicateResource { device: DeviceId, kind: ResourceKind, role: ResourceRole },
    DuplicateDmaDomain(DmaDomainId),
    DanglingDmaDomain { device: DeviceId, domain: DmaDomainId },
    DanglingDependency { device: DeviceId, provider: DeviceId },
    InvalidMmioRange { device: Option<DeviceId>, role: ResourceRole },
    InvalidIrq { device: DeviceId, role: ResourceRole },
    InvalidDmaConstraints(DmaDomainId),
    ProviderFailed { provider: ResourceProviderId, code: u32 },
    CapacityExceeded { kind: ResourceCapacityKind, required: usize },
}
```

Graph corruption is a boot-stage fatal error: no device registry or IRQ table
is published. An unsupported but valid device is not graph corruption and is
handled by the binder (§3).

---

## 3. Static drivers and the one-shot binder

<!-- txdoc:NET-DEVICE-STATIC-BINDER-1 -->

The board binary selects one local `ActiveDeviceBundle` together with
`ActivePlatform`. The bundle returns concrete, link-time descriptor slices
monomorphized for that `P`; it is not a runtime registry or HAL callback.

```rust
pub struct DriverId(pub &'static str);
pub struct MatchPriority(pub u16);

pub enum DriverMatch {
    Unsupported,
    Supported(MatchPriority),
}

pub struct StaticDriverDescriptor<P: TxPlatform> {
    pub id: DriverId,
    pub match_device: fn(&PlatformDevice) -> DriverMatch,
    pub prepare: fn(
        device: &'static PlatformDevice,
        reservation: &mut DeviceBindReservation<P>,
    ) -> Result<(), DeviceBindError>,
}

pub trait StaticDeviceBundle<P: TxPlatform>: 'static {
    fn resource_providers()
        -> &'static [ResourceProviderDescriptor<P>];
    fn drivers()
        -> &'static [StaticDriverDescriptor<P>];
}
```

The board binary's `KernelMain<P>` implementation tail-calls
`tx_kernel::kernel_main::<P, ActiveDeviceBundle>(handoff)`. This is static type
selection. There is still no `Box<dyn TxPlatform>`, runtime architecture table,
or HAL-owned driver policy.

For each enabled device, the binder evaluates every descriptor, chooses the
unique highest `MatchPriority`, and invokes only that descriptor's typed
decoder/prepare function. Equal highest matches are an error. Driver matching
must not inspect architecture name, board name, physical address, PCI placement,
discovery order, generated region name, or interface name.

`DeviceBindReservation<P>` is opaque outside device execution. It privately owns
all fallible allocations, concrete driver state, constrained DMA frame/mapping
tokens, IRQ table slots, class-registry slots, and the prospective registration.
Dropping it before commit rolls back those resources while the device remains
masked and unpublished.

```rust
pub struct DeviceBindReservation<P: TxPlatform> {
    /* private linear reservation; all platform calls monomorphize through P */
}

pub struct BoundDevice {
    pub key: BoundDeviceKey,
    pub device_id: DeviceId,
    pub driver_id: DriverId,
    pub registration: BoundDeviceRegistration,
    pub irq_contexts: &'static [DeviceIrqContext],
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BoundDeviceKey(pub u16);

pub enum BoundDeviceRegistration {
    Char(&'static CharDeviceBinding),
    Block(&'static BlockDeviceRegistration),
    Net(&'static NetDeviceRegistration),
    Controller,
}

pub struct DeviceBindReport {
    pub bound: &'static [&'static BoundDevice],
    pub unsupported: &'static [DeviceId],
    pub failed: &'static [DeviceBindFailure],
}

pub struct DeviceBindFailure {
    pub device: DeviceId,
    pub driver: Option<DriverId>,
    pub error: DeviceBindError,
}
```

Candidate-local probe/decode failures are recorded in `failed` and do not hide
a later healthy candidate. Zero supported devices is valid. Global graph,
capacity, registry-identity, or IRQ-table validation failures abort the boot
transaction and publish none of the proposed registrations.

```rust
pub enum DeviceBindError {
    AmbiguousDriver { device: DeviceId, priority: MatchPriority },
    MissingResource { device: DeviceId, kind: ResourceKind, role: ResourceRole },
    DuplicateResource { device: DeviceId, kind: ResourceKind, role: ResourceRole },
    IncompatibleResource { device: DeviceId, kind: ResourceKind, role: ResourceRole },
    Dma(DmaError),
    Irq(IrqRouteError),
    DriverProbe { code: u32 },
    RegistryCapacity { class: DeviceClass, required: usize },
    DuplicateRegistrationDevice(DeviceId),
    DuplicateDevt(DevT),
}
```

The binder is a `OneShotStepOp`. Its five stages are:

1. **observe:** read the final immutable graph and descriptor slices;
2. **upgrade/check:** validate graph references, matching uniqueness, and typed
   driver resource decoders;
3. **reserve:** allocate every fallible driver, DMA, IRQ, and registry resource
   while sources remain masked;
4. **commit:** initialize/acknowledge hardware and consume all reservations;
   commit is bounded and infallible after successful reserve;
5. **publish:** atomically install class registries and the IRQ dispatch table,
   then arm devices and unmask only their committed routes.

Visibility therefore precedes notification. No collapsed `step_init` may
allocate, publish a registration, and unmask an interrupt in one unreviewable
operation.

---

## 4. Per-device interrupt routes

<!-- txdoc:NET-DEVICE-IRQ-ROUTES-1 -->

`IrqIf` owns controller claim/complete/mask/unmask mechanics. The device binder
owns the association between one bound device, one resource role, and one
driver handler context.

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqRoute {
    pub device: DeviceId,
    pub resource: IrqResource,
}

pub struct DeviceIrqContext {
    pub route: IrqRoute,
    /// O(1) key into the immutable bound-device registry. Resolution verifies
    /// that the slot's `DeviceId` equals `route.device`.
    pub bound: BoundDeviceKey,
}

pub type DeviceIrqHandler =
    fn(context: &'static DeviceIrqContext, claimed_line: u32) -> IrqHandled;

pub struct IrqDispatchEntry {
    pub context: &'static DeviceIrqContext,
    pub handler: DeviceIrqHandler,
}

pub struct IrqDispatchBucket {
    pub handlers: &'static [IrqDispatchEntry],
}

pub struct IrqDispatchTable {
    pub entries: &'static [IrqDispatchBucket],
}
```

These dispatch types live in `tx-kernel`/the device execution seam, not in
`tx-hal`. `IrqIf` returns the claimed controller line; generic kernel IRQ code
indexes this table and invokes the stored upper-layer handler. HAL therefore
does not import `BoundDevice`, `NetDeviceOps`, or any other semantic subsystem
type.

Multiple routes may share a controller line only when every corresponding
`IrqResource` says `Shared`; otherwise reservation fails with
`IrqRouteError::ExclusiveConflict`. Repeating the same `(DeviceId,
ResourceRole)` route is always `DuplicateRoute`.

The top half calls the stored handler/context; it never finds hardware through
an interface string. For `DeferredWake`, the claimant CPU's existing deferred
slot retains the original controller line and the `DeviceIrqContext`. The same
claimant execution context performs device acknowledgement, bounded draining,
software-slot release, and exactly one `IrqIf::complete(original_line)`.

`IrqIf::uart_irq()` and `rtc_irq()` remain narrow tier-1 compatibility facts.
There is no final `NET_IRQ` or `net_irq()` surface: network routes come from the
bound device's `IrqResource`. The current singleton API is migration-only and
must be removed with the last legacy network binding path.

---

## 5. Device-scoped DMA

<!-- txdoc:NET-DEVICE-DEVICE-SCOPED-DMA-1 -->

The final `DmaIf` surface receives a domain or mapping token on every operation:

```rust
pub struct DmaMapping { /* private linear mapping token */ }

impl DmaMapping {
    pub fn domain(&self) -> DmaDomainId;
    pub fn device_addr(&self) -> DmaAddr;
    pub fn phys_addr(&self) -> PhysAddr;
    pub fn len(&self) -> usize;
}

pub enum DmaError {
    UnknownDomain(DmaDomainId),
    UnsatisfiedConstraints,
    AddressOverflow,
    SegmentBoundary,
    MappingUnavailable,
    MappingCapacity,
    InvalidRange,
}

pub trait DmaIf: PlatformConfig {
    fn map_dma(
        domain: &'static DmaDomain,
        paddr: PhysAddr,
        len: usize,
        direction: DmaDirection,
    ) -> Result<DmaMapping, DmaError>;

    fn unmap_dma(mapping: DmaMapping);

    fn sync_for_device(mapping: &DmaMapping, direction: DmaDirection);
    fn sync_for_cpu(mapping: &DmaMapping, direction: DmaDirection);
}
```

The page substrate's constrained reservation first finds a physical run that
satisfies the effective constraints (§2.2 and PAGE_SUBSTRATE §5.3). The private
bind reservation then acquires `OwnedFrameRun`, one `DmaPin` per page, and a
`DmaMapping`. Those three owners live in the concrete driver state. Failure at
any point before registry publication drops them in reverse order.

`DmaPin` proves that allocator-owned RAM remains resident; it does not prove
addressability, alignment, segment-boundary compliance, translation, or cache
coherency. `DeviceFrame` denotes non-allocator-owned MMIO and must never be used
as a DMA-buffer token.

A driver must reject an address that does not fit its descriptor. Truncation,
implicit identity translation, and platform-global coherency assumptions are
forbidden.

---

## 6. Network registration and namespace projection

<!-- txdoc:NET-DEVICE-REGISTRATION-PROJECTION-1 -->

Hardware identity and namespace identity are separate:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IfIndex(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InterfaceName {
    bytes: [u8; 16],
    len: u8,
}

pub enum InterfaceNameError { Empty, TooLong, ContainsNul }

impl InterfaceName {
    pub fn try_from_bytes(name: &[u8]) -> Result<Self, InterfaceNameError>;
    pub fn as_bytes(&self) -> &[u8];
}

pub struct NetDeviceRegistration {
    pub device_id: DeviceId,
    pub devt: DevT,
    pub ops: &'static dyn NetDeviceOps,
}

pub struct NetInterfaceProjection {
    pub ifindex: IfIndex,
    pub name: InterfaceName,
    pub device: &'static NetDeviceRegistration,
}
```

The boot transaction validates the full proposed network slice for duplicate
`DeviceId`, duplicate `DevT`, and capacity before publishing it once. It does
not synthesize a NIC when the slice is empty. Namespace projection later
assigns ifindex and name. A unique firmware alias may influence the name;
otherwise deterministic `DeviceId` ordering is allowed. Hardware, IRQ, DMA,
and driver paths continue to use `DeviceId`/registration references and cannot
depend on that name.

Names need not be stable across namespace policy changes. Selection records
should prefer `DeviceId`; interface name and ifindex are accepted only as
explicit namespace-local selectors.

---

## 7. Authoritative network configuration

<!-- txdoc:NET-DEVICE-CONFIGURATION-OWNERSHIP-1 -->

The NIC driver publishes packet transport capabilities only. It does not own
IP addresses, prefixes, routes, neighbors, DNS, proxies, DHCP state, or QEMU
topology defaults. HAL owns none of them.

Static userspace commands, DHCP-produced values, and boot/test configuration
all call the same authorized network-namespace operations for:

- link state;
- address/prefix replacement;
- route/FIB replacement;
- neighbor replacement or resolution.

DNS remains userspace-owned (for example a resolver configuration file).
BusyBox `udhcpc` is a userspace DHCP client; txKernel supplies generic packet
and control-plane ABI and does not implement a board-specific DHCP protocol in
the kernel.

### 7.1 Boot-device selection

<!-- txdoc:NET-DEVICE-BOOT-SELECTION-1 -->

```rust
pub enum BootNetSelector {
    Device(DeviceId),
    InterfaceName(InterfaceName),
    IfIndex(IfIndex),
}

pub enum BootNetSelectionError {
    NoOperationalDevice,
    UnknownDevice(DeviceId),
    UnknownInterfaceName(InterfaceName),
    UnknownIfIndex(IfIndex),
    FirmwareSelectionInvalid,
    Ambiguous { candidates: usize },
}
```

Selection order is normative:

1. use an explicit scenario/boot selector;
2. otherwise use one unique firmware-selected operational candidate, if the
   firmware contract supplies such a fact;
3. otherwise use the only operational candidate;
4. otherwise return `NoOperationalDevice` or `Ambiguous` and leave boot
   networking unconfigured.

There is no first-device, `eth0`, familiar-address, architecture, or board-name
fallback.

### 7.2 BootNetRuntime

<!-- txdoc:NET-DEVICE-BOOT-RUNTIME-1 -->

`BootNetRuntime` stores a namespace handle and the selected interface identity.
For each transmit or connection decision it queries current namespace/FIB
state for source address, route, and neighbor resolution. It does not retain a
private platform address, gateway, neighbor table, or staging NIC. Absence of
an address or route is a normal typed unconfigured result.

QEMU addresses, prefixes, host services, device placement, and expected values
belong to a versioned external scenario record. One renderer supplies QEMU
arguments, guest boot input, host-side services, and test expectations. Shared
kernel, driver, and verification code receives these values as inputs.

---

## 8. Target composition and the LA real-board hook

<!-- txdoc:NET-DEVICE-TARGET-COMPOSITION-1 -->

- **RV64 QEMU:** the selected platform publishes firmware-derived seeds; the
  linked VirtIO-MMIO descriptor decodes each matching device's own MMIO, IRQ,
  and DMA resources.
- **LA64 QEMU:** the selected platform publishes the PCI host seed; a linked
  one-shot PCI provider enumerates functions after mappings exist; the linked
  VirtIO-PCI descriptor consumes the enumerated function's BDF, BARs, route,
  and DMA domain.
- **VisionFive 2:** the selected platform publishes firmware-derived JH7110
  resources; a generic DWMAC descriptor and StarFive resource decoder/glue
  consume the matching MAC's MMIO, IRQ, DMA, clock, reset, syscon, MDIO/PHY,
  and MAC-source records.
- **Future LA real board:** a new platform crate implements the same
  `PlatformInfoIf::platform_info() -> &'static PlatformInfo`, the board binary
  selects that `P: TxPlatform` plus a local
  `ActiveDeviceBundle: StaticDeviceBundle<P>` over only the required linked
  providers and drivers. Generic binder, IRQ, DMA, namespace/FIB, and Git
  verification code is unchanged.

The future LA host workflow is a separate xtask target profile for build
artifact selection, boot transport, serial discovery, image handling, and
scenario rendering. Those are host facts, not HAL or kernel policy.

Until a concrete board exists, no target, boot protocol, NIC identity, driver,
MMIO address, IRQ, or success claim is registered for it. An
architecture-neutral synthetic platform can publish complete, missing,
reordered, and empty graphs through the exact `PlatformInfoIf` seam. That test
proves extensibility only. An empty graph is valid; an explicit request to use
networking returns `NoOperationalDevice`.

---

## 9. Failure and test contract

<!-- txdoc:NET-DEVICE-FAILURE-TEST-CONTRACT-1 -->

Required host tests cover:

- zero, one, and multiple devices;
- reversed device/resource/compatible order and unrelated decoys;
- duplicate identities, resource roles, DMA domains, and IRQ routes;
- missing mandatory resources and ambiguous highest-priority driver matches;
- failed first candidate followed by a healthy candidate;
- exclusive and shared IRQ validation, per-device acknowledgement, and
  deferred completion on the claimant context;
- direct/managed DMA domains, address limits, alignment, segment boundaries,
  coherent/non-coherent transitions, and unsatisfiable allocations;
- renamed interfaces with unchanged device/IRQ behavior;
- explicit, firmware-unique, single-candidate, absent, and ambiguous boot
  selection;
- a synthetic additional platform provider requiring no generic-kernel branch.

Required target witnesses mutate device placement and order and include a
decoy at any formerly assumed location. TLS/Git acceptance uses certificate
verification and supplied remote/CA/time inputs; invalid time, invalid CA, and
missing route must fail without verification bypass.

## 10. Implementation entry order

<!-- txdoc:NET-DEVICE-IMPLEMENTATION-ENTRY-ORDER-1 -->

1. Add portability lint and mutation/scenario fixtures.
2. Land HAL resource types, graph builder/freeze, and synthetic provider tests.
3. Land constrained DMA reservation and per-device IRQ dispatch contexts.
4. Land the one-shot binder and atomic static registries.
5. Migrate RV64 QEMU, then LA64 QEMU; remove each old fallback in the same
   milestone.
6. Make namespace/FIB state the only boot-network truth and freeze the LA hook.
7. Add generic DWMAC plus StarFive glue and validate VisionFive 2.
8. Run hermetic and real certificate-verified Git acceptance.

No DWMAC implementation begins by bypassing these common contracts.

---

## References

<!-- txdoc:NET-DEVICE-REFERENCES-1 -->

- [`HAL_v1.md`](../01_substrate/HAL_v1.md)
- [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md)
- [`DEVICE.md`](DEVICE.md)
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md)
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md)
- OASIS VirtIO 1.2 — transport/device protocol constants only.
- Devicetree Specification and each consumed binding — firmware identity and
  named-resource semantics.
- PCI/PCIe specifications — function identity, BARs, and interrupt routing.
