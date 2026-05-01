# Devices

<!-- txdoc:06-DEVICES-DEVICE -->

**Status.** v1 (2026-04-24). Draft.

**Purpose.** Specify the device subsystem for txKernel. Devices on our targets divide into three tiers with sharply different machinery: tier 1 lives in the HAL below the object model, tier 2 is statically composed per-board with no zone-allocated entities, and tier 3 (the dynamic matching framework) is deferred. This document fixes the tier-1 and tier-2 surface, names the four routes by which device data reaches userspace through VFS, and leaves tier 3 as a closed placeholder with a shape sketch.

**Audience.** Board-bringup authors, driver-crate authors, reviewers auditing what needs `Cap` and what does not, anyone wondering where `i_fops` went.

**Targets.** qemu-riscv64-virt, qemu-loongarch64, VisionFive 2 (JH7110), 2K1000LA.

**Companion documents.**

- [`CONCEPTS_v4.md`](../00_meta-framework/CONCEPTS_v4.md) — reference hierarchy and authoritative bindings. The tier-2 static-table decision is consistent with §3: `'static` references sit outside the reference hierarchy because there is no slot to pin.
- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) §3 (entities), §8.1.1 (bifurcation). Tier-2 bindings are *not entities*; tier 3's `DynamicCharDevice` (deferred) would be.
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — BIF-*, PRED-*, SIG-*. This document introduces no new invariants; it uses existing ones and explicitly notes where the entity-centric rules do not apply.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2 — `RNodeBacking`; §11.1 retires `i_fops` / `i_ops`. This document specifies what replaces them for devices, including a **revision to `StructPayload::CharDevice`** (§5.2) to hold `&'static CharDeviceBinding` rather than `Cap<CharDeviceBinding>`.
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) §3.9 — **closes the Device/Driver placeholder rows** with tier-2-specific attachments (§10).
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — the four-module layout. The device subsystem follows it in a reduced form; see §8.
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — wires hosted on `&'static` tier-2 structs need `const` constructors; see §12.3.
- [`TTY.md`](./TTY.md) — the one subsystem that genuinely wants dynamic (`Cap`-based) identity, and explains why.
- [`BDEV_FS.md`](../05_filesystem/BDEV_FS.md) — the filesystem that turns registered `BlockDevice`s into `/dev/vda`-style RNodes.

### Zone-derived type policy

<!-- txdoc:DEVICE-ZONE-DERIVED-TYPE-POLICY-1 -->

DEVICE is mostly the negative case for the policy-zone rule:

| Device declaration | Public type | Reclamation role |
|---|---|---|
| Tier-1 HAL device | platform static interface | below object model, no zone |
| Tier-2 board device binding | `&'static CharDeviceBinding` / `&'static BlockDeviceRegistration` | static fact, no zone or cap |
| devfs RNode | `Cap<RNode>` owned by VFS/PageBacked | zone-derived file identity, not device identity |
| Page-backed device content | `Cap<PageContainer>` when a driver builds one at init | page-content entity, owned by PAGE_BACKED |
| Pseudo-device schema | `&'static PseudoDevSchema` | projection schema, no zone |
| Deferred tier-3 discovered device | future `DynamicDeviceIdentity` / `DynamicDevicePayload` | full identity/payload split when implemented |

The static tier-1/tier-2 path intentionally does not use hidden RC or EBR
policies because there is no reclaimable semantic entity. Tier 3, when added,
will use the same role-shaped policy-zone interface as other upper subsystems.

---

## 1. Motivation

<!-- txdoc:DEVICE-MOTIVATION-1 -->

Linux's device model — `struct device`, `struct device_driver`, `struct bus_type`, `struct file_operations` injected per-open — solves problems txKernel does not have:

- **Hot-plug of mandatory devices** (USB sticks pretending to be the root device).
- **Heterogeneous bus composition** (ACPI + PCI + platform + USB + I²C + …).
- **Driver unloading as a user-facing operation.**

Our target set is three boards with known-at-compile-time peripherals plus qemu-virt. On these targets, the set of devices is a property of the board, the driver choice for each is a property of the board, and both are fixed at the moment the kernel image is linked. No enumeration is required to discover them; no matching is required to claim them.

Linux has the machinery it has because it ships one kernel image across every supported architecture, every supported peripheral, every supported bus topology. We do not. A per-board kernel image is fine, and it lets us collapse most of the device model into a static table.

This document formalizes that collapse while preserving one narrow escape hatch — tier 3 — for the case where we eventually need runtime discovery (PCIe slots on VJ2, future hot-swap stories). Tier 3 is deferred, not denied; its shape is sketched in §9 so that adding it later does not require reshaping what we build now.

---

## 2. The three tiers

<!-- txdoc:DEVICE-THE-THREE-TIERS-1 -->

Every device in txKernel falls into exactly one of three tiers. The tier determines *where the code lives*, *what kind of thing the device is* in the object model, and *when it comes into existence*.

### 2.1 Tier 1 — HAL devices

<!-- txdoc:DEVICE-TIER-1-HAL-DEVICES-1 -->

**Examples.** Interrupt controller (PLIC / LoongArch ExtIOI), timer (CLINT / stable timer), early console UART (for `printk` before the substrate is up).

**Characteristics.**

- Exist before `substrate::init()` returns. No zones, no heap, no `Cap<T>`.
- Addressed by compile-time-constant MMIO base. No enumeration.
- Have no devfs presence. They are not files. Kernel code reaches them through HAL function calls, never through fds.
- Not entities. Not bindings. Not pillars. Just memory and interrupt lines.

**Home.** `tx-hal-<arch>-<board>/*`. Same platform crate family as trap handling, pmap primitives, and the early console.

**Interface.** Static HAL calls through the selected platform axis — for example `<P as TimeIf>::read_ns()`, `<P as IrqIf>::claim()`, `<P as ConsoleIf>::write_bytes(&[u8])`, normally written as `P::read_ns()`, `P::claim()`, and `P::write_bytes()` when the bound is in scope. No object-model involvement.

**Wrapping.** Some tier-1 devices have a tier-2 wrapping. The early UART used for `printk` is wrapped by a tier-2 `CharDeviceBinding` after substrate init, so `/dev/console` exists and userspace can open it. The tier-1 poke-MMIO path remains as a panic-time fallback; normal I/O goes through the tier-2 binding.

### 2.2 Tier 2 — static devices

<!-- txdoc:DEVICE-TIER-2-STATIC-DEVICES-1 -->

**Examples.** Per-board on-chip peripherals: UARTs, SD/MMC controllers, GMAC Ethernet, SDHCI, RTC. Plus qemu's virtio devices at known MMIO addresses.

**Characteristics.**

- Created at tier-2 init, which runs once after `substrate::init()` returns and before userspace starts.
- The set of devices is declared in a static table in the board crate.
- Each device is represented by an `&'static` struct. Not zone-allocated. Not refcounted. Cannot be reclaimed.
- Appear in `/dev` via the devfs projection (§6).
- The driver's ops are a `&'static` vtable (`CharDeviceOps`, `BlockDeviceOps`). One vtable per driver type, shared across all instances.

**Home.** Three places:

- `drivers/<driver-crate>/` — driver crates, each defining `pub static <n>_OPS: CharDeviceOps = ...;` and per-instance state types.
- `boards/<board>/devices.rs` — per-board instantiation of `CharDeviceBinding` / `BlockDeviceRegistration` statics referencing driver vtables and chip-specific state.
- `frame/device/` — the subsystem skeleton that consumes the static tables and publishes them to devfs (§8).

**Not an entity.** Per object_model §3, entities are zone-allocated things with identity retention, reclamation, and possibly bifurcation into Identity/Payload. A tier-2 `CharDeviceBinding` is none of these. It is a configuration record with attached behavior — closer to a Linux `__initdata` driver table than to a `struct device`. The pillar pattern (BIF / PRED / WIT / OBL / SIG) does not apply because there is no zone slot to audit.

### 2.3 Tier 3 — discovered devices

<!-- txdoc:DEVICE-TIER-3-DISCOVERED-DEVICES-1 -->

**Examples (aspirational).** Anything plugged into the VJ2 PCIe slot at runtime. USB devices. Hot-plugged cards.

**Status.** **Deferred.** No tier-3 machinery ships in v1. §9 records the shape so that introducing it later is additive, not restructuring.

**Why deferred.** The targets we care about reach "working busybox + gcc + nginx" with tier 1 + tier 2 alone. SSH-over-Ethernet for debugging real boards is tier-2 (on-chip GMAC). The class of devices that genuinely requires tier 3 — USB peripherals, plug-in PCIe cards — is not on the critical path.

**Interaction with tier 2.** Bus enumerators (PCIe, USB) that would produce tier-3 nodes do not exist in v1. The PCIe host controller itself, when present as an on-chip peripheral, is tier-2; it is registered but not walked.

---

## 3. The static-binding commitment

<!-- txdoc:DEVICE-THE-STATIC-BINDING-COMMITMENT-1 -->

**DEV-1 (no dynamic bindings for tier 1 or tier 2).** Tier-1 and tier-2 devices are represented exclusively by `&'static` references. No `Cap<T>`, no `PayloadCap<T>`, no zone allocation. This applies to:

- Driver vtables (`CharDeviceOps`, `BlockDeviceOps`, `NetDeviceOps`).
- Per-driver-instance state (`Ns16550aState`, `VirtioBlkState`, ...) — defined as `static`s in the board crate.
- The binding structs that tie a devt/name to a driver (`CharDeviceBinding`, `BlockDeviceRegistration`).
- Pseudo-device projection schemas (`NULL_SCHEMA`, `ZERO_SCHEMA`).

**Rationale.** Entities in the object model exist so that reclamation can be reasoned about. Tier-1 and tier-2 devices on our targets do not reclaim — they live as long as the kernel does. Making them entities would add `Cap`/`PayloadCap` machinery with nothing for it to count. The pillar audit in prior design discussion confirmed this directly: the BIF bifurcation pattern applies to entities admitting `structural ⟂ payload` with independent reclamation lifetimes, and tier-2 devices admit neither an independent structural lifetime nor an independent payload lifetime.

**Consequence for RNodes.** The RNodes for tier-2 device nodes are still zone-allocated (RNodes always are) and carry `Cap<RNode>` in the usual ways. But the RNode's backing field stores a plain `&'static` reference into the tier-2 table — not a Cap. See §5.2 for the PAGE_BACKED revision.

**Corollary (no device-side ENODEV-on-unplug).** Because tier-2 devices cannot disappear, operations on open fds against them cannot return ENODEV for reasons of driver death. They can return ENODEV, EIO, etc. for operation-specific reasons the driver decides, but the device-vs-driver-lifecycle error class does not exist in v1.

**Tier 3 exception.** When tier 3 is added (§9), discovered devices will be zone-allocated with the full Identity/Payload factoring. The static-binding commitment applies only to tier 1 and tier 2.

---

## 4. Device classes

<!-- txdoc:DEVICE-DEVICE-CLASSES-1 -->

Every device belongs to exactly one class. The class determines which of the four dispatch routes (§5) the device uses.

| Class | Representation | Dispatch route | Userspace shape |
|---|---|---|---|
| **Char** | `&'static CharDeviceBinding` | StructBacked → `CharDeviceOps` vtable | byte stream |
| **Block** | `&'static BlockDeviceRegistration` | PageBacked via `bdev-fs` FsPageBacking | offset-keyed bytes |
| **Net** | `&'static NetDeviceRegistration` | not VFS — consumed by socket subsystem | socket API |
| **PageBackedDevice** | `Cap<PageContainer>` produced at init with `PageContainerKind::Device` | PageBacked directly, no filesystem | mmap-first |
| **Pseudo** | `&'static PseudoDevSchema` | Projected | projection-defined |

Class is not a runtime discriminant on a single type — each class has its own struct type. It's the shape of the table the board file points at.

**Pseudo** covers `/dev/null`, `/dev/zero`, `/dev/full`, and any future entries whose "driver" is a few tens of lines of state-free code. These are not drivers in the usual sense; they're projections into the content axis of PAGE_BACKED.

**PageBackedDevice** is mostly one case: a framebuffer. The producing tier-2 driver builds a `PageContainer` with `Device { base_ppn, page_count, device: <registration ref> }` at init and installs it in a fixed RNode. Opens of the devfs entry return fds on that pre-existing RNode. Reads / writes / mmap go through uniform page-backed dispatch; no ops vtable is involved.

Net is class-separate because it does not route through VFS file operations at all. `NetDeviceRegistration` is consumed by the socket subsystem's packet layer (future smoltcp integration), not by any devfs code. It is mentioned here for completeness; the detailed net-device contract lives in the net subsystem spec.

---

## 5. The four dispatch routes

<!-- txdoc:DEVICE-THE-FOUR-DISPATCH-ROUTES-1 -->

When userspace calls `read`, `write`, `mmap`, `ioctl`, etc. on an fd opened against a `/dev/*` entry, the syscall script dispatches on the RNode's `RNodeBacking` variant per PAGE_BACKED §2.1. There are four distinct routes a device RNode can take through that dispatch. This section names each.

### 5.1 Route A — Projected (pseudo-devices)

<!-- txdoc:DEVICE-ROUTE-PROJECTED-PSEUDO-DEVICES-1 -->

```
RNodeBacking::Projected { schema: &NULL_SCHEMA, key }
```

Ops are function pointers in the `PseudoDevSchema`. The schema is `&'static` in `frame/device/project.rs`. The projection key is the devfs devt or similar identifier.

**Used by.** `/dev/null`, `/dev/zero`, `/dev/full`, any future trivial pseudo-device.

**Example.**

```rust
// frame/device/project.rs
pub static NULL_SCHEMA: PseudoDevSchema = PseudoDevSchema {
    step_read:  |_, _, _| StepOutcome::Done(0),      // EOF
    step_write: |_, _, n| StepOutcome::Done(n),      // discard, count as success
    step_poll:  |_, _| PollMask::readable() | PollMask::writable(),
    ..PseudoDevSchema::EMPTY
};

pub static ZERO_SCHEMA: PseudoDevSchema = PseudoDevSchema {
    step_read:  pseudo_zero_read,                    // fills with the well-known zero frame
    step_write: |_, _, n| StepOutcome::Done(n),
    step_poll:  |_, _| PollMask::readable() | PollMask::writable(),
    ..PseudoDevSchema::EMPTY
};
```

No driver, no instance state, no MMIO. Pure interpretation.

### 5.2 Route B — StructBacked::CharDevice

<!-- txdoc:DEVICE-ROUTE-B-STRUCTBACKED-CHARDEVICE-1 -->

```
RNodeBacking::StructBacked { payload: CharDevice(&UART0_BINDING) }
```

**Revision to PAGE_BACKED §2.** `StructPayload::CharDevice` carries `&'static CharDeviceBinding`, *not* `Cap<CharDeviceBinding>`. This is the one type-signature change required by DEV-1.

```rust
// PAGE_BACKED §2, revised
pub enum StructPayload {
    Pipe(Cap<PipeIdentity>),
    Socket(Cap<SocketIdentity>),
    Tty(Cap<TtyIdentity>),                      // see TTY.md for why Tty stays dynamic
    EventFd(Cap<EventFdData>),
    TimerFd(Cap<TimerFdData>),
    SignalFd(Cap<SignalFdData>),
    Epoll(Cap<EpollData>),
    CharDevice(&'static CharDeviceBinding),     // ← tier-2 static reference
}
```

The dispatch call becomes a direct function-pointer invocation, no upgrade:

```rust
// scripts/file_io.rs (excerpt)
StructPayload::CharDevice(binding) => {
    (binding.ops.step_read)(binding, of, buf, len, ctx)
}
```

The binding struct:

```rust
// frame/device/structure/char.rs
pub struct CharDeviceBinding {
    pub devt: DevT,
    pub name: FixedName<32>,
    pub class: DeviceClass,                    // always Char here
    pub ops: &'static CharDeviceOps,
    pub driver_state: &'static (dyn Any + Send + Sync),
    pub mmio_regions: &'static [MmioRegion],
    pub irqs: &'static [IrqHandle],
    pub readable_wire: RawQueue,               // static-backed; see §12.3
    pub uevent_wire: RawPort,                  // static-backed
}

pub struct CharDeviceOps {
    pub step_read:  fn(&'static CharDeviceBinding, &OpenFile, UserBuf, usize, &Ctx) -> StepOutcome<usize>,
    pub step_write: fn(&'static CharDeviceBinding, &OpenFile, UserBuf, usize, &Ctx) -> StepOutcome<usize>,
    pub step_ioctl: fn(&'static CharDeviceBinding, &OpenFile, u32, usize, &Ctx) -> StepOutcome<i64>,
    pub step_poll:  fn(&'static CharDeviceBinding, &OpenFile) -> PollMask,
    pub step_mmap:  Option<fn(&'static CharDeviceBinding, &OpenFile, u64, usize, u32) -> StepOutcome<Cap<PageContainer>>>,
}
```

**Used by.** `/dev/ttyS*_raw` (rare — normally wrapped by TTY), `/dev/random`, `/dev/urandom`, `/dev/input/*`, `/dev/kmsg`, and the internal char-device handle that a hardware TTY's `TtyTransport::Hardware` refers to.

**Instance state.** Lives in `driver_state: &'static dyn Any`. Each concrete driver provides a `static <n>_STATE: <DriverStateType> = ...;` in the board crate (or `pub static` in the driver crate if truly stateless). Internal mutability (atomics, `Mutex`) is the norm because `&'static` does not allow `&mut` access.

### 5.3 Route C — PageBacked via bdev-fs

<!-- txdoc:DEVICE-ROUTE-C-PAGEBACKED-VIA-BDEV-FS-1 -->

```
RNodeBacking::PageBacked { pc: Cap<PageContainer> }
    where pc.kind = PageContainerKind::File { fs: bdev_fs, fs_object_id: devt.into() }
```

Block devices are modeled as files in a pseudo-filesystem called **bdev-fs** (specified in [`BDEV_FS.md`](../05_filesystem/BDEV_FS.md)). The dispatch is the *uniform* PageBacked path — page cache, mmap, read, write all work without any device-specific code — and the only custom piece is bdev-fs's `FsPageBacking` implementation, which translates `(fs_object_id, offset)` to block-device I/O.

```rust
// frame/device/structure/block.rs
pub struct BlockDeviceRegistration {
    pub devt: DevT,
    pub name: FixedName<32>,
    pub ops: &'static BlockDeviceOps,
    pub driver_state: &'static (dyn Any + Send + Sync),
    pub mmio_regions: &'static [MmioRegion],
    pub irqs: &'static [IrqHandle],
    pub uevent_wire: RawPort,
    pub io_complete_wire: RawQueue,            // fires on request completion from IRQ
}

pub struct BlockDeviceOps {
    pub step_read_blocks:  fn(&'static BlockDeviceRegistration, u64 /*lba*/, u32 /*n*/, &mut [Frame]) -> StepOutcome<()>,
    pub step_write_blocks: fn(&'static BlockDeviceRegistration, u64, u32, &[Frame]) -> StepOutcome<()>,
    pub step_barrier:      fn(&'static BlockDeviceRegistration) -> StepOutcome<()>,
    pub total_blocks:      fn(&'static BlockDeviceRegistration) -> u64,
    pub block_size:        fn(&'static BlockDeviceRegistration) -> u32,
}
```

The `BlockDevice` trait specified in [`TX_EXT4_PLAN_v1_2.md §3.2`](../05_filesystem/TX_EXT4_PLAN_v1_2.md) for tx-ext4's consumption is equivalent to `BlockDeviceOps`'s function-pointer form, wrapped behind an adapter if trait dispatch is preferred in tx-ext4.

**Used by.** `/dev/vda`, `/dev/vdb`, `/dev/sda`, `/dev/mmcblk0`, `/dev/nvme0n1`, and partition devices (which are slices of parent block devices; see BDEV_FS).

### 5.4 Route D — PageBacked Device kind (framebuffer)

<!-- txdoc:DEVICE-ROUTE-D-PAGEBACKED-DEVICE-KIND-FRAMEBUFFER-1 -->

```
RNodeBacking::PageBacked { pc: Cap<PageContainer> }
    where pc.kind = PageContainerKind::Device { device: <reg ref>, base_ppn, page_count }
```

No filesystem, no ops vtable. The PageContainer page index is pre-populated at device init with Frames whose PPNs reflect the device's MMIO region. Reads, writes, and mmap go through the uniform PageBacked path; `fetch_page` is trivial because every page is already present.

**Used by.** `/dev/fb0`. Possibly future direct-mmap MMIO windows.

**Producer.** The framebuffer driver at tier-2 init calls a `frame::device::register_framebuffer(devt, name, mmio_base, page_count)` helper that constructs the `PageContainer`, installs its Frames, and publishes the RNode into devfs with `PageBacked { pc }` backing.

### 5.5 Route selection at `open` time

<!-- txdoc:DEVICE-ROUTE-SELECTION-OPEN-TIME-1 -->

`open("/dev/X")` walks devfs (§6), which materializes an RNode with one of the four backings above, per the device's class. The backing is **fixed at RNode creation** — consistent with PAGE_BACKED §2.1's commitment to no open-time injection.

Subsequent `read`/`write`/`mmap`/`ioctl` through `fd` against that RNode dispatches in `scripts/file_io.rs` (or the relevant syscall script) by matching on `RNodeBacking`. The match arms for device-relevant variants are:

```rust
match &rnode.backing {
    RNodeBacking::PageBacked { pc } => {
        // Route C (bdev-fs) and D (framebuffer) both land here.
        page_backed::step_read(pc, of, buf, len, ctx)
    }
    RNodeBacking::StructBacked { payload: StructPayload::CharDevice(binding) } => {
        // Route B.
        (binding.ops.step_read)(binding, of, buf, len, ctx)
    }
    RNodeBacking::StructBacked { payload: StructPayload::Tty(tty_id) } => {
        // TTY dispatches into frame::tty first; hardware TTYs may internally
        // invoke a CharDeviceBinding's ops through TtyTransport::Hardware.
        tty::step_read(tty_id, of, buf, len, ctx)
    }
    RNodeBacking::Projected { schema, key } => {
        // Route A (pseudo-devices) shares this arm with procfs/sysfs/devpts.
        (schema.step_read)(key, of, buf, len, ctx)
    }
    // ... non-device arms ...
}
```

---

## 6. The devfs filesystem

<!-- txdoc:DEVICE-THE-DEVFS-FILESYSTEM-1 -->

### 6.1 What devfs is

<!-- txdoc:DEVICE-WHAT-DEVFS-IS-1 -->

**devfs is the projection of the tier-2 device registry onto a VFS namespace.** It is a filesystem instance in the same sense as tmpfs or ext4: it has a `MountPayload`, an `FsOps` impl, and appears at `/dev` after the init script mounts it.

It is **not** itself an entity, beyond the `MountIdentity`/`MountPayload` pair that every mount has. All of its namespace contents are derived from the static device tables at runtime — the same way procfs derives its contents from the live process set.

### 6.2 Directory structure

<!-- txdoc:DEVICE-DIRECTORY-STRUCTURE-1 -->

```
/dev/
    null, zero, full                    — pseudo (Route A)
    random, urandom                     — char (Route B)
    ttyS0, ttyS1, ...                   — tty (wraps Route B internally; see TTY.md)
    console                             — tty alias (see TTY.md)
    pts/
        0, 1, 2, ...                    — pty slaves (see TTY.md)
        ptmx                            — pty multiplexer
    vda, vda1, vda2, ...                — block (Route C)
    sda, sda1, ...                      — block (Route C)
    mmcblk0, mmcblk0p1, ...             — block (Route C)
    fb0                                 — framebuffer (Route D)
    input/
        event0, event1, ...             — char (Route B)
    kmsg                                — char (Route B)
```

### 6.3 Lookup

<!-- txdoc:DEVICE-LOOKUP-1 -->

`FsOps::lookup(parent_fs_object_id, name)` consults the tier-2 registry:

1. If `parent` is root `/dev` and `name` matches a registered pseudo-device, char device, block device, framebuffer, or TTY by name, return its fs_object_id (which is the devt packed into a u64).
2. If `parent` is `/dev/pts` and `name` parses as a pty-slave index that exists, return that.
3. If `parent` is `/dev/input` and `name` matches a registered input-class char device, return that.
4. Otherwise ENOENT.

The registry scan is a linear walk over the static `BINDINGS` slices (one per class). Linear is fine: the total count is on the order of 20 per board.

### 6.4 RNode materialization

<!-- txdoc:DEVICE-RNODE-MATERIALIZATION-1 -->

When devfs constructs an RNode for a looked-up device, the backing is picked per class:

| Device class | RNode backing |
|---|---|
| Pseudo | `Projected { schema: <static schema ref>, key: (class, devt) }` |
| Char | `StructBacked { payload: CharDevice(<static binding ref>) }` |
| Block | `PageBacked { pc: <Cap on bdev-fs's pre-built PC for this device> }` |
| Framebuffer | `PageBacked { pc: <Cap on the driver-built Device-kind PC> }` |
| TTY (hw or pty) | `StructBacked { payload: Tty(<Cap on the TtyIdentity>) }` |

Multiple opens of the same devfs path produce *distinct* RNodes pointing at the same underlying binding / PC / TtyIdentity. There is no devfs-wide RNode coherence index for device entries: each entry's semantic identity is the thing it points at (binding ref or Cap), not the RNode.

**Exception: bdev-fs PCs must be shared.** Two opens of `/dev/vda` must share the page cache, which means they must share the PC. bdev-fs maintains a `devt → Cap<PageContainer>` map inside its MountPayload so that the same Cap is handed to each devfs materialization. See BDEV_FS.

### 6.5 Mutation operations

<!-- txdoc:DEVICE-MUTATION-OPERATIONS-1 -->

Most FsOps operations on devfs return errors:

- `create_inode`, `mkdir`, `symlink`, `link`: `EROFS`. Userspace cannot create entries in `/dev` in v1. (This diverges from Linux's `mknod`; we do not plan to support it.)
- `unlink`, `rmdir`: `EROFS`.
- `rename`: `EROFS`.
- `readdir`: works; enumerates the registry.
- `lookup`, `load_inode_meta`: work as described.
- `destroy_inode`: unreachable; devfs RNodes have no persistent state to destroy.

Future udev-equivalent functionality, if it ever lands, would run in userspace and populate a separate tmpfs overlay — not mutate devfs.

### 6.6 Unlinked-but-open

<!-- txdoc:DEVICE-UNLINKED-BUT-OPEN-1 -->

Since devfs never unlinks, the unlinked-but-open window does not arise. An fd held across the kernel's lifetime continues to work because the underlying tier-2 binding is `&'static` and cannot go away.

---

## 7. Initialization and boot ordering

<!-- txdoc:DEVICE-INITIALIZATION-BOOT-ORDERING-1 -->

The device subsystem comes up in a strict order relative to substrate, VFS, and userspace. Each phase's preconditions are enforceable: calling a phase's routine before its preconditions hold is a bug.

### 7.1 Phase table

<!-- txdoc:DEVICE-PHASE-TABLE-1 -->

| Phase | When | What happens | Preconditions |
|---|---|---|---|
| **0** | Bootloader → kernel entry | Tier-1 HAL-device init: PLIC / timer / early UART. Enables `printk`. | Nothing |
| **1** | `substrate::init()` runs | Zones, frame allocator, slab. Builds `Cap<T>` machinery. | Phase 0 |
| **2** | `vfs::init()` runs | Root tmpfs mounted; VFS operational for tier-2 registration to construct PCs. | Phase 1 |
| **3** | `device::init()` runs | Walks the board's static BINDINGS tables; invokes each driver's `step_init` to poke registers, claim IRQs, allocate DMA buffers. After this, all tier-2 devices are live. | Phase 2 |
| **4** | `devfs::init()` runs | Mounts devfs at `/dev`. Lookups start resolving. | Phase 3 |
| **5** | `bdev_fs::init()` runs | Mounts bdev-fs. Block-device PCs become constructible. | Phase 3 |
| **6** | `tty::init()` runs | Tier-2 TTYs registered against their underlying char bindings. `/dev/ttyS0`, `/dev/console` become openable. | Phase 3, 4 |
| **7** | `exec::init_userspace()` | `init` runs. Userspace can open `/dev/*`. | Phase 6 |

### 7.2 The board's role

<!-- txdoc:DEVICE-THE-BOARD-S-ROLE-1 -->

Each supported board provides:

```rust
// boards/<board>/devices.rs

/// Instance state for each device, as `static`s.
pub static UART0_STATE: Ns16550aState = Ns16550aState { mmio_base: 0x1000_0000, ... };
pub static VIRTIO_BLK0_STATE: VirtioBlkState = VirtioBlkState { mmio_base: 0x1000_1000, ... };
// ...

pub static CONSOLE_READABLE_WIRE: StaticRawQueue = StaticRawQueue::new();
pub static CONSOLE_UEVENT_WIRE: StaticRawPort = StaticRawPort::new();

/// Bindings: one per tier-2 device, referencing driver vtables + per-instance state.
pub static UART0_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(MAJ_TTY_S, 0),
    name: FixedName::from_bytes(b"ttyS0_raw"),    // wrapped by TTY; ttyS0 is the wrapper
    class: DeviceClass::Char,
    ops: &ns16550a::OPS,
    driver_state: &UART0_STATE,
    mmio_regions: &[MmioRegion { base: 0x1000_0000, len: 0x100 }],
    irqs: &[IrqHandle { plic_num: 10 }],
    readable_wire: CONSOLE_READABLE_WIRE.raw(),
    uevent_wire: CONSOLE_UEVENT_WIRE.raw(),
};

pub static VIRTIO_BLK0_REGISTRATION: BlockDeviceRegistration = BlockDeviceRegistration { ... };

// ...

/// The tables device::init() walks.
pub static CHAR_BINDINGS: &[&'static CharDeviceBinding] = &[
    &UART0_BINDING,
    &RANDOM_BINDING,
    // ...
];

pub static BLOCK_REGISTRATIONS: &[&'static BlockDeviceRegistration] = &[
    &VIRTIO_BLK0_REGISTRATION,
];

pub static PSEUDO_DEVICES: &[PseudoDeviceEntry] = &[
    PseudoDeviceEntry { devt: DevT::new(MAJ_MEM, 3), name: "null",  schema: &NULL_SCHEMA },
    PseudoDeviceEntry { devt: DevT::new(MAJ_MEM, 5), name: "zero",  schema: &ZERO_SCHEMA },
    PseudoDeviceEntry { devt: DevT::new(MAJ_MEM, 7), name: "full",  schema: &FULL_SCHEMA },
];
```

`device::init()` iterates these slices and calls a per-class registration helper on each entry, which:

- Adds the binding reference to the device subsystem's class-specific index (a small slice sidecar keyed by devt).
- Invokes `binding.ops.step_init(binding)` if the driver declares one, to poke registers and claim IRQs.
- Wires `binding.irqs` into the PLIC handler table, with the handler calling into driver code that fires `binding.readable_wire` (or equivalent) on completion.

Everything runs once and never again.

### 7.3 What tier 3 would add

<!-- txdoc:DEVICE-WHAT-TIER-3-WOULD-ADD-1 -->

A tier-3 implementation (deferred, §9) would run *after* phase 7 or as a reactor-scheduled task. It would walk buses (PCIe, USB) to discover new nodes, match them to dynamically-registered drivers, and publish the results into a parallel zone-allocated device registry that devfs's lookup would additionally consult. Tier 3 does not disturb the tier-1/tier-2 machinery above.

---

## 8. Module layout

<!-- txdoc:DEVICE-MODULE-LAYOUT-1 -->

The device subsystem follows `SUBSYSTEM_ANATOMY` §1 in reduced form, because it has no zone-allocated entities (DEV-1).

```
frame/device/
    structure/
        char.rs               CharDeviceBinding, CharDeviceOps
        block.rs              BlockDeviceRegistration, BlockDeviceOps
        net.rs                NetDeviceRegistration, NetDeviceOps
        pseudo.rs             PseudoDevSchema, PseudoDeviceEntry
        mmio.rs               MmioRegion, IrqHandle (shared address-range/IRQ types)
        index.rs              class-keyed registry slices (e.g. `CHAR_BINDINGS`)
                              that mirror the board's exports; populated at device::init
    checks/                   (empty — no witness production for &'static tables;
                               the ref itself is the evidence.)
    execution/
        step_init.rs          device::init() — walks board tables, claims IRQs.
        step_char_read.rs     Helper wrappers that adapt driver step_* functions
        step_char_write.rs    to the script-side dispatch signature, if needed.
                              (Mostly unnecessary — the driver's ops are called
                              directly from scripts/file_io.rs.)
    project.rs                Devfs lookup/readdir projection:
                              (class, name) → RNodeBacking construction.
                              Pseudo-device schema definitions (NULL_SCHEMA, etc.)
                              live here or in a sibling pseudo_schemas.rs.

drivers/
    ns16550a/                 Driver crates. Each defines:
        ops.rs                  static OPS: CharDeviceOps = ...;
        state.rs                struct Ns16550aState { ... }
        irq.rs                  IRQ handler body
    virtio_blk/
    virtio_net/
    ...

boards/
    qemu_riscv64_virt/
        devices.rs            Per-board BINDINGS tables (see §7.2).
        irq_map.rs            PLIC interrupt number assignments.
    visionfive2/
        devices.rs
        irq_map.rs
    loongson_2k1000la/
        devices.rs

frame/devfs/
    structure.rs              Trivial — devfs MountPayload is a marker type.
    checks/                   (empty)
    execution/
        step_mount.rs         One-shot mount at boot.
    fs_ops.rs                 impl FsOps for DevfsInstance — lookup/readdir as §6.

frame/bdev_fs/
    (see BDEV_FS.md)
```

### 8.1 What's in `structure/` vs the board crate

<!-- txdoc:DEVICE-WHAT-S-STRUCTURE-BOARD-CRATE-1 -->

- `structure/` defines *types* (`CharDeviceBinding` the struct, `CharDeviceOps` the struct). No instances.
- `boards/<board>/devices.rs` defines *instances* (`static UART0_BINDING: CharDeviceBinding = ...`). No types.
- `drivers/<crate>/` defines *vtables and state types* (`static OPS: CharDeviceOps = ...; struct Ns16550aState { ... }`). Vtables are type-generic; state is per-driver.

The three-way split matches Tock's [kernel / chips / boards] layout, applied to our [types / drivers / boards] axis. Each is a Cargo crate boundary in the workspace.

---

## 9. Tier 3 (deferred)

<!-- txdoc:DEVICE-TIER-3-DEFERRED-1 -->

This section is a **shape sketch**, not a specification. It names the types and decisions that would be needed to add dynamic device discovery later, so that the absence of those types in v1 does not leave implicit assumptions that would be violated by their addition.

### 9.1 What tier 3 adds

<!-- txdoc:DEVICE-WHAT-TIER-3-ADDS-1 -->

- A zone `dyn_char_device` holding `DynamicCharDeviceIdentity` / `DynamicCharDevicePayload` pairs. These *are* entities, with full Identity/Payload bifurcation per object_model §8.1.1.
- A runtime driver-registration system. Each dynamic driver crate declares a `DriverDescriptor` with a `matches(&PropertyBag) -> bool` predicate and a `probe(node) -> Result<DriverInstance>` entry point. Bind-rule-ish, Fuchsia DFv2-shaped, but simpler (no IDL, no IPC, no process isolation).
- Bus enumerators: `substrate::bus::pci` (walks PCIe ecam), `substrate::bus::usb` (xHCI host driver consumer), possibly `substrate::bus::platform_dt` for DT-walking on boards that want it.
- A `uevent_port` publication from each `DynamicCharDeviceIdentity` for userspace udev-equivalent code.

### 9.2 Where it plugs in

<!-- txdoc:DEVICE-WHERE-IT-PLUGS-1 -->

- **devfs.** `FsOps::lookup` additionally consults the dynamic registry after the tier-2 static tables return ENOENT.
- **RNodeBacking.** A new variant `StructPayload::DynamicCharDevice(Cap<DynamicCharDeviceIdentity>)` is added. Existing tier-2 `CharDevice(&'static ...)` path is unchanged.
- **Unplug.** When a dynamic device payload drops, its outstanding fds observe ENODEV via the Identity/Payload upgrade failure — the normal pattern used by Mount, Socket, Process.

### 9.3 Why defer

<!-- txdoc:DEVICE-WHY-DEFER-1 -->

The incremental work is bounded and self-contained. Bus enumerators, bind rules, and the dynamic registry can be added in a future v2 without reshaping anything written here. No tier-2 driver crate needs modification; no board file needs modification; the script-side dispatch gets one new match arm.

Deferring avoids building machinery before knowing what it's for — the target workload (Linux 2.6 parity for busybox / gcc / nginx / ssh) has no tier-3 requirement.

---

## 10. Signal attachments

<!-- txdoc:DEVICE-SIGNAL-ATTACHMENTS-1 -->

This section **closes `SIGNAL_ATTACHMENTS.md §3.9`'s placeholder rows** for tier 1 and tier 2. Tier 3's attachments will be added when tier 3 ships.

### 10.1 Attachments hosted on `&'static` tier-2 bindings

<!-- txdoc:DEVICE-ATTACHMENTS-HOSTED-STATIC-TIER-2-BINDINGS-1 -->

`CharDeviceBinding` and `BlockDeviceRegistration` host wires for readiness and hot-plug-style events, even though tier-2 devices do not themselves hot-plug. The wires exist because:

- Drivers fire `readable_wire` from IRQ handlers to wake poll/epoll subscribers.
- `uevent_port` fires once, at `device::init()` time, so that any userspace `udev` watching the port sees the devices it would have seen in Linux (a niceness for porting existing userland; the actual attachment of new devices happens only during init in v1).

Because these wires live on `&'static` hosts, they must be constructed by `const` constructors. See §12.3.

### 10.2 Catalog rows

<!-- txdoc:DEVICE-CATALOG-ROWS-1 -->

| Entity | Carrier | Wire | Transition | Polarity | Fired from | Subscribers | Projection link |
|---|---|---|---|---|---|---|---|
| `CharDeviceBinding` (static) | RawQueue | `readable_wire` | Driver-observed data available (IRQ path) | `set(HasData)` | driver IRQ handler → `binding.readable_wire.fire(...)` | poll/select/epoll | — (readiness hint, not projection change) |
| `CharDeviceBinding` (static) | RawQueue | `writable_wire` (optional, per driver) | Driver TX ring has space | `set(HasSpace)` | driver IRQ handler or step-write path | poll/select/epoll | — |
| `CharDeviceBinding` (static) | RawPort | `uevent_port` | Device registered at `device::init()` | `fire(UEvent::Add{devt, name})` | `device::step_init` | udev-equivalent (if any) | — (one-shot at boot) |
| `BlockDeviceRegistration` (static) | RawQueue | `io_complete_wire` | Disk I/O request completed | `set(Complete{tag})` | driver IRQ handler | in-flight coroutines waiting in `step_read_blocks` / `step_write_blocks` | — |
| `BlockDeviceRegistration` (static) | RawPort | `uevent_port` | Device registered at `device::init()` | `fire(UEvent::Add{devt, name})` | `device::step_init` | udev-equivalent | — |

### 10.3 BIF-5 check

<!-- txdoc:DEVICE-BIF-5-CHECK-1 -->

Every row above names a single host (`CharDeviceBinding` or `BlockDeviceRegistration`, both `&'static` and therefore trivially in a single retention domain — the "always-retained" domain). No wire is split across Identity/Payload, because there is no Identity/Payload split at tier 2 (DEV-1). BIF-5 holds vacuously: single-host attachment is the only kind possible.

### 10.4 Tracepoints

<!-- txdoc:DEVICE-TRACEPOINTS-1 -->

Driver step functions emit tracepoints per the `tracepoint!` macro model, declared at the driver crate level:

```rust
// drivers/ns16550a/tracepoints.rs
tracepoint! {
    ns16550a {
        rx_irq(binding_devt: u64, bytes: usize),
        tx_complete(binding_devt: u64, bytes: usize),
        fifo_overrun(binding_devt: u64),
    }
}
```

These are aggregated under the tracing catalog's `(any) tracing` row in `SIGNAL_ATTACHMENTS §3.10`; no additional rows here.

---

## 11. Per-target device inventories

<!-- txdoc:DEVICE-PER-TARGET-DEVICE-INVENTORIES-1 -->

Target-specific tables. The board crate for each target provides exactly these devices at v1.

### 11.1 qemu-riscv64-virt

<!-- txdoc:DEVICE-QEMU-RISCV64-VIRT-1 -->

| Path | Class | Driver | Notes |
|---|---|---|---|
| `/dev/null`, `/dev/zero`, `/dev/full` | Pseudo | (none) | Route A |
| `/dev/random`, `/dev/urandom` | Char | `random` (chacha-seeded) | Route B |
| `/dev/kmsg` | Char | kernel log ring | Route B |
| `/dev/ttyS0`, `/dev/console` | TTY (hw) | `ns16550a` at 0x1000_0000 | TTY wraps Char |
| `/dev/vda`, `/dev/vda1`, ... | Block | `virtio_blk` at virtio-mmio bus | Route C, partitions via BDEV_FS |
| `/dev/fb0` | Framebuffer | `virtio_gpu` | Route D; deferred if display not needed |
| `/dev/input/event0` | Char | `virtio_input` | Route B; optional |

Net: `virtio_net` provides a `NetDeviceRegistration`; does not appear in `/dev`.

### 11.2 qemu-loongarch64

<!-- txdoc:DEVICE-QEMU-LOONGARCH64-1 -->

Same inventory as qemu-riscv64-virt modulo IRQ controller (LoongArch ExtIOI instead of PLIC) and architecture-specific trap/timer code in HAL. Driver crates (virtio-*) are shared.

### 11.3 VisionFive 2 (JH7110)

<!-- txdoc:DEVICE-VISIONFIVE-2-JH7110-1 -->

| Path | Class | Driver | Notes |
|---|---|---|---|
| `/dev/null`, `/dev/zero`, `/dev/full` | Pseudo | (none) | |
| `/dev/random`, `/dev/urandom` | Char | `random` | |
| `/dev/kmsg` | Char | kernel log ring | |
| `/dev/ttyS0`, `/dev/console` | TTY (hw) | `jh7110_uart` (8250-variant) | |
| `/dev/mmcblk0`, `/dev/mmcblk0p*` | Block | `dw_sdhci` (DesignWare SDHCI) | Gnarly chip-specific init; longest pole in VJ2 bringup |
| `/dev/eth0` | (Net) | `jh7110_gmac` + `yt8521_phy` | Not in /dev; consumed by net subsystem |

USB (DWC2 / xHCI) and PCIe slot devices are **tier 3 (deferred)**. The PCIe host controller is registered as tier 2 but its enumeration walk does not run in v1.

### 11.4 Loongson 2K1000LA

<!-- txdoc:DEVICE-LOONGSON-2K1000LA-1 -->

Shape equivalent to VJ2 with LoongArch-specific register maps. UART, SD/MMC, and Ethernet all have tier-2 drivers; USB and PCIe-slot devices deferred.

---

## 12. Summary of retirements and revisions

<!-- txdoc:DEVICE-SUMMARY-RETIREMENTS-REVISIONS-1 -->

This document makes the following changes to prior specs. Each is a tightening that existing text allows without contradiction; no breaking invariant change is introduced.

### 12.1 `PAGE_BACKED_v1.md §2` — `StructPayload::CharDevice` field type

<!-- txdoc:DEVICE-PAGE-BACKED-V1-MD-2-STRUCTPAYLOAD-CHARDEVICE-FIELD-1 -->

```
-  CharDevice(Cap<CharDeviceBinding>),
+  CharDevice(&'static CharDeviceBinding),
```

Rationale: DEV-1. A companion note in PAGE_BACKED §11.2 mentioning the retained `CharDeviceBinding` as `Cap`-held is updated to reflect the `&'static` form.

### 12.2 `SIGNAL_ATTACHMENTS.md §3.9` — Device/Driver placeholder rows

<!-- txdoc:DEVICE-SIGNAL-ATTACHMENTS-MD-3-9-DEVICE-DRIVER-PLACEHOLDER-1 -->

Replaced with §10.2 above. The "Device" host is now `CharDeviceBinding` (`&'static`); the "DeviceNode" host is resolved into either `CharDeviceBinding` or `BlockDeviceRegistration` depending on class.

### 12.3 `BUS.md` — static wire storage

<!-- txdoc:DEVICE-BUS-MD-CONST-CONSTRUCTORS-1 -->

Added requirement: bus wires used by static device/block tables must be backed
by const-constructible storage. The current implementation provides
`StaticRawQueue` and `StaticRawPort` as the storage objects; static
registrations embed `RawQueue` / `RawPort` handles produced by
`STATIC_WIRE.raw()` or `RawQueue::from_static(&STATIC_WIRE)` /
`RawPort::from_static(&STATIC_WIRE)`. This keeps the hot `RawQueue` /
`RawPort` fire/subscribe API unchanged while avoiding allocation for
statically declared devices. Internal mutability (atomics, locks, or
intrusive subscriber-list heads) is required for the wire state, since the
enclosing struct is `&'static` and cannot be `&mut`-accessed.

Static device and block declarations should use the bus's first declaration
macro slice (`bus_readiness!` / `bus_lifecycle!`) for typed readiness and
lifecycle bit sets, then pair those types with `DeclaredQueue<E>` /
`DeclaredPort<E>` when subsystem-facing validation is needed. Tracepoint
payload structs can now use `bus_tracepoint!`; trace subscriber/nop-patching
runtime remains later bus work.

Dynamic zone/device owners that embed bus wires use the bus owner-manifest
path instead of static storage. The owner type implements `WireOwnerManifest`,
retires every embedded queue/port under one epoch guard, and calls
`retire_wire_owner<T>()` so the typed owner reclaim callback is queued through
EBR only after all embedded wires are terminal-drained. Owners with a simple
embedded-wire field list can generate the manifest through
`bus_wire_owner_manifest!`. Concrete device owner manifests remain part of the
later dynamic device/VFS integration, not the static registration rows above.

---

## 13. Open questions

<!-- txdoc:DEVICE-OPEN-QUESTIONS-1 -->

**13.1 Whether `driver_state: &'static dyn Any` is the right level of type erasure.** Alternatives: generics on `CharDeviceBinding` (ugly across the registry slice), a driver-side-only concrete type with a cast helper (cleaner at the driver site, same erasure at the subsystem site). No correctness difference; ergonomic decision deferred to implementation.

**13.2 Major/minor number allocation policy.** v1 hard-codes devt values in the board file. A small "major-number registry" (much like Linux's `Documentation/admin-guide/devices.txt` but generated at compile time from the board's table) would catch collisions at build time. Nice-to-have; not blocking.

**13.3 `/sys/*` (sysfs).** Not covered here. Sysfs is a projection of the same registry with different content (device attributes rather than device nodes). If userspace tooling requires it (some udev builds do), it lands as a parallel projection filesystem, roughly the same size as devfs. Default: not built until something demands it.

**13.4 Framebuffer alignment with DRM.** For v1, framebuffer = one Device-kind PC, bit-blit via mmap. Full DRM (KMS, GEM, DMA-buf) is a non-goal. If we get to graphical userland, this becomes an open question.

---

## References

<!-- txdoc:DEVICE-REFERENCES-1 -->

- [`CONCEPTS_v4.md`](../00_meta-framework/CONCEPTS_v4.md) §2.5, §3.
- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) §3, §8.1.1.
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — BIF-*, PRED-*, SIG-*.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2, §11.
- [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md) — frame allocator, FrameMeta, slab.
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) §3.9 (closed by §10 above).
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — const-constructor requirement per §12.3.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — reduced form per §8.
- [`TTY.md`](./TTY.md) — consumer of `CharDeviceBinding` via `TtyTransport::Hardware`.
- [`BDEV_FS.md`](../05_filesystem/BDEV_FS.md) — block-device filesystem.
- [`TX_EXT4_PLAN_v1_2.md`](../05_filesystem/TX_EXT4_PLAN_v1_2.md) §3.2 — `BlockDevice` trait this document reframes as `BlockDeviceOps`.
