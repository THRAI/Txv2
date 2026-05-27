# Object Pattern Audit — v1

<!-- txdoc:00-META-FRAMEWORK-OBJECT-PATTERN-AUDIT-V1 -->

**Status.** Object-pattern cleanup audit for implementation.

**Purpose.** Check whether objects already described in subsystem docs follow the current txKernel object pattern before implementation. This is not a replacement for [`object_model_v2.md`](object_model_v2.md); it is the input checklist for subsystem cleanup.

**Companion.** [`OBJECT_PATTERN_FIXES_v1.md`](OBJECT_PATTERN_FIXES_v1.md) details each `OPA-*` violation and proposes concrete fixes.

**Pattern checked.**

- An entity is either co-located, identity/payload split, compound-payload, identity-only, static/outside-object-model, or binding value.
- Addressability bindings target identity; operational evidence reaches payload through declared evidence.
- Derived materializations name an authoritative binding.
- Projection/backing filesystems do not become shadow truth stores.
- Services may own durable ledgers without becoming full semantic subsystems.

---

## 1. Verdict

<!-- txdoc:OBJECT-PATTERN-AUDIT-VERDICT-1 -->

The object pattern is strong enough to rewrite from. Most major objects are already consistent:

- Process, Mount, and TTY correctly use `Identity` / `Payload` split for zombie, lazy-umount, and hangup cases.
- PageContainer, AddressSpace, DEntry, RNode, OpenFile-like objects are mostly co-located or compound-payload as expected.
- VM's `VmEntry` is correctly a binding value, not a separate object.
- bdev-fs, tmpfs, procfs, devfs, devpts, and tx-ext4 mostly respect filesystem-instance boundaries.
- Tier-1/tier-2 devices correctly sit outside the zone/ref object model.

The main work is not inventing a new object model. It is applying the current one precisely enough to absorb the few stale or ambiguous cases below.

---

## 2. Must Fix Before Implementation

<!-- txdoc:OBJECT-PATTERN-AUDIT-MUST-FIX-1 -->

**OPA-1. PageBacked has stale `StructPayload` variants.**

Source: [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2.

Current stale shapes:

```rust
StructPayload::Tty(Cap<TtyData>)
StructPayload::CharDevice(Cap<CharDeviceBinding>)
PageContainerKind::Device { device: Cap<DevNode>, ... }
```

These conflict with later docs:

- [`TTY.md`](../06_devices/TTY.md) makes TTY `TtyIdentity` / `TtyPayload`, with RNodes carrying `StructBacked::Tty(Cap<TtyIdentity>)`.
- [`DEVICE.md`](../06_devices/DEVICE.md) makes tier-2 char devices `&'static CharDeviceBinding`, not `Cap<CharDeviceBinding>`.
- Tier-2 devices do not define `DevNode` entities.

Object-model implication: `object_model_v2.md` / `EBR_ZONE_INTERFACE_v1.md` define "static backing reference" as outside `Cap<T>`, and `PAGE_BACKED_v1` needs a follow-up correction.

**OPA-2. `Cap<MountPayload>` vs `PayloadCap<MountPayload>` vocabulary is ambiguous.**

Sources: [`MOUNT_v1.md`](../05_filesystem/MOUNT_v1.md), [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md), [`BDEV_FS.md`](../05_filesystem/BDEV_FS.md).

Mount's conceptual model is correct: `MountIdentity` carries `PayloadBinding<MountPayload>`, and operational users pin the payload after lazy detach. But several docs write `Cap<MountPayload>` where the pattern likely wants payload evidence / `PayloadCap<MountPayload>` / `MountPayloadPin`.

Object-model implication: the current canonical spelling is:

- either payloads are zone entities that may be retained by `Cap<Payload>`, with `PayloadCap` as the role name;
- or direct `Cap<Payload>` should be retired from prose in favor of `PayloadCap<Payload>`.

The rule needs to be explicit because PageContainer file backing and OpenFile mount pins depend on it.

**OPA-3. TTY controlling-session/pgrp binding disagrees with PROCESS entities.**

Sources: [`TTY.md`](../06_devices/TTY.md) §2.2 and §4; [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) §2.3-2.4.

PROCESS defines `ProcessGroup` and `Session` as identity-only entities. TTY's `SessionPgrp` instead stores leader `Cap<ProcessIdentity>` values and describes session/pgrp as view keys. That can be made valid, but only if it is explicitly a derived materialization over PROCESS-owned bindings.

Object-model implication: the current model includes identity-only entities and view-key/materialization rules. TTY should either:

- bind to `Cap<Session>` and `Cap<ProcessGroup>`, or
- keep leader caps but name the authoritative PROCESS bindings and revalidation rule.

**OPA-4. Pid namespace objects need final classification.** Resolved by [`NAMESPACE_VIEW_v1.md`](NAMESPACE_VIEW_v1.md).

Source: [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) §9.

Earlier `PidNamespace` sketches used direct maps to process/thread/group/session identities while `PidStruct` was also referenced by `ProcessIdentity`, `ProcessGroup`, and `Session`. The resolved model makes `PidNamespace.numbers` the authoritative numeric signifier index and routes entries through `PidName` / `PidStruct`; target identities carry non-retaining snapshots only.

Object-model implication: the current model should classify:

- `PidNamespace`: PROCESS-owned namespace/view entity for pid/tid/pgid/sid signifier bindings.
- `PidName` / `PidStruct`: namespace binding value and future multi-namespace indirection, not canonical topology.

This matters because `PidNamespace.numbers -> Cap<PidName> -> target identity` retains the target for addressability without making pid numbers part of parent/pgrp/session topology.

---

## 3. Mostly Correct, Keep As Examples

<!-- txdoc:OBJECT-PATTERN-AUDIT-KEEP-EXAMPLES-1 -->

**OPA-OK-1. Process uses the split pattern well.**

Source: [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) §2.

`ProcessIdentity` persists through zombie; `ProcessPayload` drops at process exit. Wait-addressability and kill-addressability are distinct projections. This is the canonical POSIX example for why identity retention must not imply payload retention.

**OPA-OK-2. Mount is the best ARCH-5 object case study.**

Source: [`MOUNT_v1.md`](../05_filesystem/MOUNT_v1.md).

`MountNamespace.mountpoint_index` is the authoritative crossing binding. `MountIdentity.mountpoint`, parent/child links, and mount-table views are consistency/materialization fields. Lazy umount cleanly separates namespace reachability from payload liveness.

Use this as the non-VM case study for `object_model_v2.md` examples and implementation review.

**OPA-OK-3. VM keeps binding values separate from entities.**

Source: [`VM_v1_2.md`](../03_memory-vm/VM_v1_2.md).

`AddressSpace` is a co-located entity. `VmEntry` is the authoritative binding value inside `recipes`, not an entity with its own lifecycle. PTEs are derived materializations justified by current recipes.

**OPA-OK-4. PageContainer is a clean co-located content object.**

Source: [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §3.

`PageContainer` has no degraded-but-addressable state and no namespace of its own. Its VM-local page index owns cache pins; reclaim is driven by final retention drop and bounded reclaim work.

**OPA-OK-5. tmpfs/bdev-fs/tx-ext4 respect filesystem-instance boundaries.**

Sources: [`bringup_fs_specs_v_1 (1).md`](<../05_filesystem/bringup_fs_specs_v_1 (1).md>), [`BDEV_FS.md`](../05_filesystem/BDEV_FS.md), [`TX_EXT4_PLAN_v1_2.md`](../05_filesystem/TX_EXT4_PLAN_v1_2.md).

- tmpfs owns backend-private `TmpfsNode` records inside its `MountPayload`; it does not own DEntry/RNode/OpenFile.
- bdev-fs contributes one `MountPayload` and no new entity classes; its `devt -> Weak<PageContainer>` coherence index is a binding, not retention.
- tx-ext4 avoids `Cap<RNode>` and keeps per-inode decoded state out of the backend, matching the filesystem-instance pattern.

**OPA-OK-6. Device tiering is a valid object-model carve-out.**

Source: [`DEVICE.md`](../06_devices/DEVICE.md).

Tier-1 and tier-2 devices are `&'static` board facts, not zone entities. Dynamic tier-3 devices are deferred and would need the full identity/payload model. This is a clean exception to document in v3.

**OPA-OK-7. Scheduler and reactor avoid semantic entity ownership.**

Sources: [`REACTOR_v0.md`](../02_execution/REACTOR_v0.md), [`SCHEDULER_v0.md`](../02_execution/SCHEDULER_v0.md).

`TaskHandle`, `TaskId`, queues, and scheduler metadata are execution mechanisms, not semantic objects. The docs mostly preserve the boundary: thread/runtime carries semantic identity; reactor/scheduler carry dispatch state.

---

## 4. Needs Clarification, Not Necessarily Wrong

<!-- txdoc:OBJECT-PATTERN-AUDIT-CLARIFICATIONS-1 -->

**OPA-Q-1. Service objects need a compact rule.**

Sources: [`cred_service_v_1_draft (2).md`](<../02_execution/cred_service_v_1_draft (2).md>), [`rlimit_service_v_1_draft (1).md`](<../02_execution/rlimit_service_v_1_draft (1).md>).

`Credential`, `RLimitBag`, and `RLimitUsage` are retained by `Cap<T>` and live under ProcessPolicy, but they are service ledger objects, not full subsystems. The model should state that service ledger objects may be co-located semantic entities without namespace projections.

**OPA-Q-2. Frame/FrameMeta needs separate "physical object" wording.**

Source: [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md).

Frames have compound payload counters (`refcount`, `map_count`, `cache_ref`, pin-like state), but frame allocation is substrate/foundation-adjacent. The implementation should treat `Frame` as a substrate physical object that follows the same proof shape with a different allocation base.

**OPA-Q-3. Native fd adapters in SIGNAL need VFS alignment.**

Source: [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md) §36-37.

`SignalFd` and `PidFd` are described as zone-allocated entities with `PayloadBinding`. In the newer PageBacked/VFS direction, fd adapters should probably be `RNodeBacking::StructBacked` payloads or VFS-hosted RNodes with subsystem-owned targets. This is not necessarily wrong, but the ownership home should be made explicit.

**OPA-Q-4. `OpenFile` / fd-table object shapes are referenced but not canonically specified.**

Sources: [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md), [`EXEC_v1.md`](../02_execution/EXEC_v1.md), [`MOUNT_v1.md`](../05_filesystem/MOUNT_v1.md).

`OpenFile`, `FdTable`, `FdEntry`, `FsContext`, cwd/root mount pins, and fd close publication are load-bearing, but their object classification is scattered. VFS structure should own their classification, with `object_model_v2.md` examples covering the pattern.

---

## 5. Recommended Inputs To Object-Model Examples

<!-- txdoc:OBJECT-PATTERN-AUDIT-OBJECT-MODEL-INPUTS-1 -->

Add these categories explicitly:

| Category | Definition | Examples |
|---|---|---|
| Co-located entity | identity and payload coincide | `PageContainer`, `AddressSpace`, likely `DEntry`, `OpenFile` |
| Identity/payload split | identity may outlive payload | `ProcessIdentity/ProcessPayload`, `MountIdentity/MountPayload`, `TtyIdentity/TtyPayload`, `ThreadIdentity/ThreadPayload` |
| Identity-only entity | addressable semantic identity with no operational payload | `ProcessGroup`, `Session` |
| Compound-payload entity | payload projection is a disjunction of typed contributors | `Frame`, `RNode`, possibly inode-like VFS objects |
| Binding value | stored in an authoritative container; no independent identity | `VmEntry`, `TmpfsNode` if backend-private, directory entries inside backend maps |
| Filesystem-instance object | hosted by `MountPayload`, owns backend state but not VFS topology | tmpfs, bdev-fs, tx-ext4, procfs, devfs, devpts |
| Service ledger entity | durable policy/accounting object with no namespace | `Credential`, `RLimitBag`, `RLimitUsage` |
| Static object-model carve-out | `'static` board/platform fact, no reclamation | tier-1/tier-2 devices, HAL tables |
| Execution object | reactor/scheduler mechanism, not semantic truth | `TaskHandle`, `WaitToken`, scheduler queues, completion private channel |

Also add a rule for payload evidence spelling:

> Direct references to a payload object must be described as operational evidence for the owning identity. Docs should not casually use `Cap<Payload>` unless v3 defines that as a valid implementation spelling of `PayloadCap<Payload>`.

---

## 6. Rewrite Blockers

<!-- txdoc:OBJECT-PATTERN-AUDIT-REWRITE-BLOCKERS-1 -->

Before declaring the subsystem docs implementation-ready, either fix or explicitly defer:

- `OPA-1`: stale PageBacked `StructPayload` / `Device` variants.
- `OPA-2`: canonical payload evidence spelling.
- `OPA-3`: TTY session/pgrp binding target.
- `OPA-4`: PidNamespace/PidStruct classification.

Everything else can be captured as examples or follow-up cross-doc edits.

See [`OBJECT_PATTERN_FIXES_v1.md`](OBJECT_PATTERN_FIXES_v1.md) for the recommended patch order.
