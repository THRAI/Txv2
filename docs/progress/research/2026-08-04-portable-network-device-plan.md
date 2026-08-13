# Portable network-device and real HTTPS clone plan

Date: 2026-08-04

Status: active; Phase 0 and Phase 1 completed, Phase 2 is in progress.

This plan supersedes
[`2026-08-04-vf2-gmac-github-clone-plan.md`](2026-08-04-vf2-gmac-github-clone-plan.md).
It replaces the earlier single-board bring-up sequence with one resource-driven
path for four explicit operating modes: RV64 QEMU, LA64 QEMU, VisionFive 2,
and a deferred LA real-board extension hook. The LA board model, boot protocol,
and network controller are intentionally unspecified until that board is
connected.

## Outcome

The final system must discover and bind the network hardware described by the
current machine, apply network policy supplied outside generic kernel code, and
complete a certificate-verified HTTPS clone whose repository is a test input.
Changing a device address, interrupt route, PCI location, device order,
interface name, network topology, or test repository must not require editing a
generic kernel or driver source file.

The current delivery implements and verifies the two QEMU modes and the RV
real-board mode. It also freezes a compile/link-time LA real-board seam so the
fourth mode can be filled in later without editing generic kernel code. It does
not pretend that an unknown LA board is already supported.

Implementation readiness is now **yes for Phase 1/Phase 2**. Phase 0 defined the
multi-resource graph, static composition bundle, one-shot binder, per-device
IRQ/DMA, network configuration ownership, and LA extension seam. The detailed
audit is
[`2026-08-04-portable-network-phase0-readiness.md`](2026-08-04-portable-network-phase0-readiness.md).
DWMAC remains later in the entry order; this readiness result does not claim it
is already implemented.

## No-environment-hardcoding contract

“No hardcoding” in this plan means that deployment and topology facts are data,
not driver logic.

### Prohibited in production and shared harness code

- Board MMIO addresses, interrupt numbers, PCI bus/device/function placement,
  BAR placement, PHY addresses, and device enumeration order.
- A global or singleton network interrupt, a fixed network interface name, or
  selecting a device because it was discovered first.
- A literal MAC address, IP address, prefix, gateway, DNS server, proxy, static
  neighbor, or default network interface.
- A QEMU-only topology or address being used as a kernel fallback.
- A fixed Git remote, branch, commit, host path, serial device, or host network
  interface in a reusable verification script.
- Assuming that firmware or a bootloader left clocks, resets, PHY state, DMA,
  interrupt masks, or link state usable.
- Guessing a missing fact. A missing required resource is a typed diagnostic and
  a clean probe failure, never a fallback to a familiar value.

### Permitted constants

Drivers must contain constants defined by a hardware or protocol specification:
register offsets, bit masks, descriptor formats, compatible identifiers, PCI
identifiers, Ethernet protocol values, and bounded algorithmic capacities.
Each such constant must be named, live in a protocol- or controller-specific
module, cite its specification family, and have a focused test where practical.

A bounded static registry is also permitted because tier-2 devices have static
lifetime. Capacity exhaustion must be explicit and diagnostic; it must not
silently discard a device or change which device is selected.

### Fixtures and captured evidence

Concrete deployment values may appear only in clearly marked captured firmware
artifacts, board logs, or test scenario fixtures. Fixtures are inputs to the
production parser and binder; production code must not import them. Mutation
tests must move the values and reorder the devices so a hidden dependency on a
known fixture fails.

Every resource fact carries a provenance category such as firmware, bus
enumeration, device capability, boot argument, DHCP, user configuration,
protocol specification, or fixture seed. This makes guessed or mis-layered
configuration reviewable.

## Architectural shape

The static platform rule remains unchanged: one `P: TxPlatform` is selected at
compile/link time. This plan does not add a runtime HAL manager, a boxed HAL
trait object, a HAL-owned semantic network object, or generic-kernel branching
on architecture or board name.

```text
firmware tables / bus enumeration
                |
                v
      immutable DeviceResourceGraph
                |
     +----------+-----------+
     | static DriverDescriptor set
     v
 typed resource decoder and one-shot binder
                |
                v
 BoundDevice(DeviceId, resources, IrqRoute, DmaDomain)
                |
                v
 NetDeviceRegistration + namespace-visible interface projection

boot arguments / DHCP / userspace configuration
                |
                v
       network namespace and FIB
                |
                v
 BootNetRuntime queries current authoritative state
```

### Immutable platform resource graph

The platform publishes a boot-built, immutable resource graph. Candidate public
types are:

- `DeviceId`: stable hardware identity derived from firmware path or bus
  identity, never from discovery order or interface name.
- `ResourceOrigin`: typed provenance for each fact.
- `PlatformDevice`: compatible/bus identifiers plus all resources belonging to
  one device.
- `MmioResource` and `IrqResource`: complete, role-named collections rather
  than one anonymous range and one anonymous interrupt.
- `DmaDomain` and `DmaConstraints`: device-visible address translation,
  addressability mask, alignment, segment boundary, coherency, and IOMMU
  domain, if present.
- Typed dependency references such as `ClockRef`, `ResetRef`, `SysconRef`,
  `MdioBusRef`, `PhyRef`, and `MacAddressSource`.

The graph is a hardware-fact channel. It owns no driver policy and contains no
untyped arbitrary property bag. A driver-specific decoder validates a device
record and produces a concrete resource type such as
`VirtioMmioResources`, `VirtioPciResources`, or
`Jh7110GmacResources`.

### Static driver set, boot-time binding

The linked kernel contains a closed static set of `DriverDescriptor`s. At boot,
the generic device initializer matches resource records against that set using
firmware compatible identifiers or bus identifiers, then creates bindings that
live for the rest of the boot.

This is a tier-2 extension, not tier-3 hotplug:

- Driver availability is fixed at link time.
- Discovery and binding happen once during boot.
- Successful registrations have static lifetime and are never detached.
- Runtime arrival, removal, unloading, and reclamation remain tier-3 work.

All matching must be independent of `P::ARCH`, physical address, PCI location,
resource-list order, or generated MMIO-region name. Every valid supported NIC
is considered. A failed candidate does not hide a later healthy candidate.

### Per-device IRQ routing

Replace the singleton network-interrupt surface with an `IrqRoute` for each
bound device and interrupt role. The route retains `DeviceId` and the driver
context needed by both halves of interrupt handling.

Required lifetime order:

1. Validate the device resources while all device sources remain masked.
2. Register the route and handler context.
3. Initialize the device and clear stale device-side status.
4. Publish the `NetDeviceRegistration`.
5. Arm the device and unmask only its routes.

The top and bottom halves use the route identity to acknowledge the same
device. They never look up hardware through a namespace interface name.
Device acknowledgement and bounded draining precede controller completion, and
the claimant execution context completes the original controller claim exactly
once.

### Device-scoped DMA

DMA allocation consumes `DmaDomain` and `DmaConstraints`; it does not assume
identity DMA, global coherency, or a fixed address ceiling. The allocator either
returns memory satisfying the device constraints or a typed failure. Drivers
never truncate an address to fit a descriptor.

Capability registers may refine firmware-provided constraints after reset.
Cache ownership transitions always go through the selected platform's DMA/cache
surface, including platforms where the implementation becomes a no-op.

### Network identity and configuration

`DeviceId` is hardware identity. An interface name is a namespace projection
assigned after registration. Naming can use a unique firmware alias or a
deterministic `DeviceId` ordering, but hardware and IRQ paths must not depend on
the resulting string.

Boot-device selection follows this order:

1. An explicit selector supplied by the boot/test scenario.
2. A unique firmware-selected candidate, when the firmware contract provides
   one.
3. The only operational candidate, when exactly one exists.
4. Otherwise, report ambiguity and leave boot networking unconfigured.

IP addresses, prefixes, routes, DNS, proxies, and neighbor entries are not
device facts. Static boot arguments, DHCP, and userspace configuration all
materialize the same network-namespace/FIB state. `BootNetRuntime` queries that
state for interface, source address, route, and neighbor resolution rather than
keeping a private platform default. DNS remains userspace-owned.

QEMU convenience defaults belong to an external scenario renderer. The same
scenario record supplies QEMU arguments, guest boot configuration, host-side
services, and test expectations so those values cannot drift between scripts.

### DWMAC and StarFive composition

The DWMAC driver is split into:

- A controller core containing only specification-defined MAC/DMA registers,
  descriptors, rings, and capability discovery.
- A StarFive glue layer consuming typed clock, reset, syscon, MDIO/PHY, MAC
  source, IRQ, MMIO, and DMA resources from the graph.
- A PHY layer selected from the discovered PHY identity and link mode.

The glue initializes from reset and does not rely on bootloader handoff state.
MAC selection follows firmware/NVMEM/board sources and validation rules; any
generated locally administered address uses the entropy service and is never a
compiled literal. Link-speed-dependent clock programming derives from the
negotiated link state and the clock-provider contract.

### Deferred LA real-board hook

The LA real-board hook is a static board-support seam, not a runtime callback
or a placeholder device. Its semantic contract is:

1. A future LA platform crate selects its concrete `P: TxPlatform` and
   publishes that board's immutable device resource graph through the same HAL
   fact surface used by the other targets.
2. The future board binary links only the static driver descriptors required by
   that board. Compatible/bus matching and binding remain in the common boot
   path.
3. A future xtask target profile supplies build artifact selection, boot
   transport, serial discovery, image handling, and scenario rendering. Those
   are host workflow facts, not kernel policy.
4. The generic device, IRQ, DMA, namespace/FIB, and Git verification code is
   unchanged when the board bundle is added.

Phase 0 must name the exact trait/type spelling for this seam. Until the board
is available, the repository must not invent its MMIO, IRQ, firmware format,
PCI topology, boot protocol, NIC identifiers, driver, or target-profile values.
No fake LA board target is registered and no unsupported target reports
success.

The hook is kept honest with an architecture-neutral test platform that
publishes synthetic resource graphs through the same interface. Tests vary and
remove those resources to prove that a future board can join without a generic
kernel edit and that missing resources fail diagnostically. This mock verifies
the extension seam only; it is not evidence that a particular LA board works.

When the board is connected, completing the fourth mode consists of capturing
its firmware/boot facts, selecting or adding its NIC driver descriptor,
implementing its platform crate and xtask profile, and running the existing
mutation and hardware acceptance ladder. A genuinely new resource shape still
requires an explicit typed-contract review; it must not be smuggled through an
untyped property bag.

## Delivery plan

### Phase 0 — make the design implementation-ready

Update the active architecture before code:

- `HAL_v1.md`: replace the flat single-device assumptions in
  `txdoc:HAL-PLATFORMINFOIF-1`, the single network route in
  `txdoc:HAL-IRQIF-TRAIT-SURFACE-1`, and the platform-global-only DMA contract
  in `txdoc:HAL-DMAIF-1` with the resource graph, per-device routes, and
  device-scoped DMA constraints.
- `DEVICE.md`: distinguish boot-discovered static-lifetime tier-2 devices from
  tier-3 hotplug; revise `txdoc:DEVICE-MOTIVATION-1`,
  `txdoc:DEVICE-TIER-2-STATIC-DEVICES-1`,
  `txdoc:DEVICE-THE-STATIC-BINDING-COMMITMENT-1`,
  `txdoc:DEVICE-PHASE-TABLE-1`, and `txdoc:DEVICE-THE-BOARD-S-ROLE-1`.
- `PAGE_SUBSTRATE_v1.md`: specify constrained contiguous DMA allocation at
  `txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-MULTI-FRAME-CONTIGUOUS-ALLOCATION-1`.
- canonical `Txv3/02_INVARIANTS_v5.md`: preserve carried-forward `MAP-2A` and
  add lintable `DEVRES-*` rules for immutable resource provenance, per-device
  IRQ ownership, device-scoped DMA, and the absence of environment
  configuration in generic code. Update superseded `INVARIANTS_v4.md` only to
  correct its compatibility wording.
- Add a canonical `NET_DEVICE`/boot-network specification under
  `docs/design/06_devices/` and link it from `docs/design/INDEX.md`; the current
  device document refers to a detailed net-device contract that does not yet
  exist.
- Define the static LA real-board extension seam and its unsupported/absent
  behavior without naming a board, boot protocol, controller, address, or IRQ.

Exit criterion: a new implementation-readiness audit reports **ready: yes** and
names the final public types, lifetimes, failure behavior, init order, and
authoritative configuration owners.

### Phase 1 — build the anti-hardcoding harness first

Add one versioned `NetworkScenario` schema and make all QEMU, host-service,
guest-command, and expected-result generation consume it. The schema stores
selectors and source categories, not duplicated platform defaults.

Add `cargo xtask lint net-portability` with positive and negative snippets. It
must reject generic-kernel architecture/board branching, singleton network IRQ
APIs, hardware lookup by a fixed interface name, production fallback devices,
deployment literals in shared network paths, fixed QEMU placement, and TLS
verification bypass. Captured firmware, explicit fixtures, and sourced protocol
constant modules are narrow, reviewed exceptions.

Add deterministic mutation fixtures before migrating production paths. Each
fixture family changes resource values and order and places a decoy at the old
location.

Exit criterion: the new tests fail against the current implementation for the
expected reasons and the lint has tests proving both rejection and permitted
protocol constants.

### Phase 2 — land the common resource, binding, IRQ, and DMA model

Implement the immutable resource graph and typed decoders. Replace the current
single-resource record, ordinal region naming, single network IRQ, and
interface-name bottom-half lookup with `DeviceId`-linked records and routes.
Generalize the network IRQ outcome and DMA ownership helpers so they are not
named after or private to VirtIO.

The registry publishes all successfully bound network devices in one atomic
boot transaction. Zero devices is valid and produces no synthetic interface.
Overflow, duplicate identity, duplicate route, missing mandatory resources, and
ambiguous binding are explicit failures.

Exit criterion: host tests cover zero/one/multiple devices, reversed order,
duplicate and missing resources, failed-first/healthy-later candidates,
per-device IRQ acknowledgement, multiple DMA constraint sets, and a synthetic
additional platform provider that needs no generic-kernel branch.

### Phase 3 — migrate RV64 QEMU without a fallback path

Discover every virtio-mmio transport from the current firmware description.
Probe the device type at the published resource, retain that device's own IRQ,
and bind block and network functions independently of node order or generated
region names.

Vary transport placement and insert unrelated transports in QEMU scenarios.
An old location containing a decoy must never be touched.

Exit criterion: RV64 boot, local hermetic HTTPS Git, existing Git networking,
and network benchmarks pass across reordered, zero-NIC, and multi-NIC
scenarios.

### Phase 4 — migrate LA64 QEMU without a fixed PCI topology

Enumerate the available PCI hierarchy and carry actual bus identity, BARs,
interrupt pin/capabilities, and routed interrupt into each device record. Bind
VirtIO by identifiers and capabilities, not by a fixed slot, first match, or
generic-kernel MMIO-region string.

QEMU scenarios move the NIC, exchange block/network order, add a second NIC,
and make the first matching candidate fail initialization.

Exit criterion: the same LA64 boot, hermetic HTTPS Git, existing Git
networking, and benchmark gates pass for every topology variant.

### Phase 5 — remove legacy paths and make portability a merge gate

Delete the architecture match in generic device init, singleton network IRQ
surface, fixed-interface registration helper, staging network fallback, ordinal
region fallback, and fixed QEMU/PCI selection paths. Do not retain a compatibility
fallback that can silently resurrect them.

Make `net-portability` and the mutation suite mandatory. A repository search
for the retired APIs and environment literals is supporting evidence, not the
sole protection; typed APIs and mutation tests remain the primary guard.

Exit criterion: both QEMU architectures use only the common binding path and
the new lint passes with no production exceptions.

### Phase 6 — separate boot networking from platform and driver code

Parse an optional external `NetBootConfig`, resolve its device selector to a
registered `DeviceId`, and apply it through the same namespace/FIB operations
used by rtnetlink or userspace. Remove private boot-runtime source-address,
route, and static-neighbor truth.

Exercise static boot arguments, userspace configuration, and a DHCP-produced
fixture as equivalent inputs. QEMU user, TAP, and bridge modes are rendered
from scenarios rather than compiled defaults.

Exit criterion: changing every network value in the scenario requires no
kernel rebuild, and packets use the namespace/FIB state currently visible to
userspace.

### Phase 7 — freeze the LA real-board extension hook

Publish and test the compile/link-time board-support seam defined in Phase 0.
The test-only provider must supply arbitrary resource graphs and a static driver
set through the common interfaces. The xtask side defines how a future target
profile plugs into build, boot, serial, image, and scenario selection, but it
does not register a fictional LA board profile.

Exit criterion: a review can enumerate the future board-only files and confirm
that adding a conforming LA board requires no edit to generic device, IRQ, DMA,
network-policy, or Git-test code. Empty and incomplete mock resources fail
cleanly instead of selecting another platform's defaults.

### Phase 8 — add generic DWMAC and StarFive glue

Implement the controller core, constrained DMA rings, PHY/MDIO lifecycle, MAC
source selection, polling diagnostics, and finally interrupts. Bind every
enabled compatible instance with complete resources; the boot scenario chooses
which operational device to use. No controller ordinal is privileged in code.

Bring-up order is cold reset and resource validation, link, changing counters,
neighbor discovery, ICMP, local TCP, local HTTP, sustained transfer, and IRQ
accounting. Polling is diagnostic only; interrupt mode is required for phase
completion.

Exit criterion: a cold board boot reaches sustained bidirectional traffic with
balanced claim/completion accounting and no dependence on bootloader network
state.

### Phase 9 — certificate-verified Git acceptance

Use the scenario to provide the board's network configuration, time source or
time-setting procedure, CA material, optional explicitly configured proxy, and
repository URL. Advance through DNS, local trusted HTTPS, remote
`git ls-remote`, and a shallow clone. Never disable certificate verification.

The real remote test is a hardware release witness, not the only CI gate:
hermetic trusted-HTTPS Git remains mandatory so an external outage cannot hide
a kernel regression. Preserve the serial log, scenario record, discovered
resource summary, and resulting commit identifier.

Exit criterion: the board completes a real remote shallow clone with TLS
verification enabled, and changing the remote or network scenario requires no
source edit.

## Verification matrix

| Area | Required variations | Required result |
|---|---|---|
| Firmware parsing | Resource values, compatible order, node order, named-resource order, enabled state | Stable `DeviceId`; exact resources; unsupported/missing facts fail without guessing |
| PCI | Bus identity, function, BAR type/address, capability order, interrupt routing, multiple devices | Binding and IRQ follow the enumerated function, independent of location and order |
| DMA | Address masks, translated domains, coherent/non-coherent modes, alignments, unavailable satisfying memory | Correct allocation and sync, or typed failure; never truncation |
| NIC count | None, one, several, reversed registration, first candidate failure | No fake NIC; all healthy NICs register; explicit ambiguity instead of first-device policy |
| IRQ | Multiple routes, reordered IRQ properties, interface-name changes | Claim, device ACK, wake, and completion remain tied to one `DeviceId` |
| Network policy | Boot static, userspace static, DHCP-produced state, no default route | Namespace/FIB is authoritative; absence remains unconfigured |
| RV64 QEMU | Reordered and relocated virtio transports, decoys, zero/multiple NICs | Same generic binder; no fixed transport assumption |
| LA64 QEMU | Relocated PCI function, changed block/network order, failed and multiple candidates | Same generic binder; no fixed PCI assumption |
| VF2 | Current boot firmware, any complete supported MAC instance, cold reset | Resources agree with firmware/capabilities; no controller-ordinal dependency |
| LA real-board hook | Synthetic additional platform with complete, missing, and reordered resources; no concrete board profile | Common binder accepts the provider without a generic branch; absence remains explicitly unsupported |
| TLS/Git | Trusted local endpoint, incorrect time/CA negative cases, parameterized real remote | Verification fails safely when invalid and succeeds without bypass when valid |
| Stress | Sustained transfer, Git and benchmarks, interrupt mode, multiple scenario seeds | No route mix-up, leaked claim, DMA violation, stall, or ordering dependency |

## Phase gates and rollback

- Preserve the current RV64 and LA64 boot/Git/benchmark evidence before Phase 2.
- Each platform migration is complete only when its old path is removed in the
  same milestone and the platform gate is green.
- A failed phase reverts to the preceding verified code; it does not add a
  guessed fallback or a second source of truth.
- Hardware work does not persist bootloader environment changes, write storage,
  or alter host routing/firewall state unless the user separately authorizes
  that operation.
- Every phase updates `docs/progress/STATUS.md`, its operational plan record,
  and the relevant architecture decision or research note.

## Scope boundaries

This plan includes four explicit operating modes: two QEMU targets, VisionFive
2, and a deferred LA real-board extension hook. It includes boot-time discovery
of fixed-lifetime devices, multiple NIC registration, deterministic selection,
per-device IRQ/DMA, external network configuration, DWMAC/StarFive support,
and the Git acceptance ladder.

It does not add hotplug, driver unloading, runtime HAL selection, a generic
untyped firmware property API, or a full tier-3 device lifecycle. DHCP client
enablement is tracked separately in
[`2026-08-04-userspace-dhcp-client-plan.md`](2026-08-04-userspace-dhcp-client-plan.md):
BusyBox owns the DHCP protocol while txKernel supplies generic AF_PACKET and
control-plane ABI. Advanced multi-interface routing policy and unrelated socket
or filesystem redesign remain separate work; this plan ensures DHCP-produced
inputs can use the common authoritative network state.

The exact LA real-board platform crate, boot workflow, NIC driver, and hardware
acceptance are deferred until the board model is known and connected. The hook
is in scope now; claiming support for an unknown board is not.

## Implementation-readiness audit

Ready: **yes for Phase 1/Phase 2 implementation**.

Closed by Phase 0:

- `HAL_v1.md` now specifies an immutable resource seed, one-shot final graph,
  controller-only IRQ surface, and domain-scoped DMA.
- `DEVICE.md` separates link-time implementation selection, boot-time
  discovery/binding, static lifetime, and deferred runtime hotplug.
- `PAGE_SUBSTRATE_v1.md` names constrained DMA run allocation and its typed
  failures.
- `NET_DEVICE_v1.md` owns exact public types, lifetimes, binder transaction,
  network identity/configuration, target composition, and LA hook.
- canonical v5 `DEVRES-*` supplies lintable rules; v4 remains a compatibility
  anchor only.

The portability lint and mutation harness are Phase 1 implementation outputs,
not missing design decisions. See the dedicated readiness audit for the
seven-axis evidence and frozen public shape.

Nonblocking follow-ups:

- Identify and implement the actual LA real board when it is connected.
- Runtime hotplug and detach.
- Driver unloading and tier-3 reclamation.
- Advanced automatic multi-uplink policy.
- Broader bus families not required by the three target environments.

Implementation entry order:

1. Phase 0 architecture contracts and readiness re-audit.
2. Phase 1 scenario/lint/negative tests.
3. Phase 2 common typed resource and lifetime model.
4. RV64 QEMU migration, then LA64 QEMU migration, then legacy removal.
5. Network-policy separation.
6. Freeze and test the deferred LA real-board hook.
7. DWMAC/StarFive implementation for the RV real board.
8. Hermetic and RV real-board Git acceptance.

## Canonical references selected for this plan

- `txdoc:CONCEPTS-V5-HOMES-1`
- `txdoc:INVARIANTS-MAP-1`, especially `MAP-2A`
- `txdoc:MODULE-MAP-FOUNDATION-HAL-1`
- `txdoc:MODULE-MAP-STATIC-REGISTRIES-1`
- `txdoc:HAL-PLATFORMINFOIF-1`
- `txdoc:HAL-IRQIF-1`
- `txdoc:HAL-DMAIF-1`
- `txdoc:DEVICE-TIER-2-STATIC-DEVICES-1`
- `txdoc:DEVICE-THE-STATIC-BINDING-COMMITMENT-1`
- `txdoc:DEVICE-INITIALIZATION-BOOT-ORDERING-1`
- `txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-MULTI-FRAME-CONTIGUOUS-ALLOCATION-1`
- `txdoc:STEP-V2-ANTI-PATTERNS-1`
