# txKernel Architecture Docs — Index

<!-- txdoc:INDEX -->

Categorization of the 40 architecture documents in this folder, grouped by the
layer of the kernel they specify. Order roughly tracks the layering: each tier
depends on the ones above it.

---

## 00 · Meta-framework

<!-- txdoc:INDEX-META-FRAMEWORK-1 -->

The architectural vocabulary every other doc references. Read these first.

- [`01_CONCEPTS_v5.md`](../Txv3/01_CONCEPTS_v5.md) — draft unified concepts rewrite: placement homes, step/script/reactor async model, middleware/protocol combinators, completion, publication rule.
- [`02_INVARIANTS_v5.md`](../Txv3/02_INVARIANTS_v5.md) — canonical grep-friendly invariant set: BIF/PRED/WIT/OBL/SIG/STEP/ASYNC/SCRIPT/COMP/ARCH plus linter notes.
- [`object_model_v2.md`](00_meta-framework/object_model_v2.md) — identity / capability / payload entity decomposition.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — four-module subsystem layout, five-stage step discipline, import rules.
- [`MODULE_MAP_v1.md`](00_meta-framework/MODULE_MAP_v1.md) — draft placement taxonomy for foundation, substrate, reactor, policy, subsystems, services, filesystem instances, scripts, shims, projections, and static registries.
- [`CI_REPORTING_v1.md`](00_meta-framework/CI_REPORTING_v1.md) — CI output shape, design-reference tags, required gates, and future boot-sentinel reporting contract.
- [`OBJECT_PATTERN_AUDIT_v1.md`](00_meta-framework/OBJECT_PATTERN_AUDIT_v1.md) — implementation cleanup audit of existing objects against the identity/payload, binding, filesystem-instance, service, and static carve-out patterns.
- [`OBJECT_PATTERN_FIXES_v1.md`](00_meta-framework/OBJECT_PATTERN_FIXES_v1.md) — detailed `OPA-*` violation analysis, proposed fixes, TTY session/pgrp repair, and PidNamespace/PidStruct design recommendation.
- [`NAMESPACE_VIEW_v1.md`](00_meta-framework/NAMESPACE_VIEW_v1.md) — nsproxy and namespace-view architecture: lens bundle, pid-name resolution, syscall commits, projected RNodes, and blast radius.

Noncanonical source-trace notes:

- [`FRAMEWORK_HARVEST_v1.md`](00_meta-framework/FRAMEWORK_HARVEST_v1.md) — staging harvest that led to the current meta-framework cleanup; not an implementation contract.
- [`INVARIANT_LEDGER_v1.md`](00_meta-framework/INVARIANT_LEDGER_v1.md) — source ledger preserved for traceability; `02_INVARIANTS_v5.md` is canonical.

Archived historical meta-framework sources:

- [`README.md`](00_meta-framework/archived/README.md) — archive note and replacement map.
- [`CONCEPTS_v3.md`](00_meta-framework/archived/CONCEPTS_v3.md) — superseded concept vocabulary.
- [`INVARIANTS_v3_3.md`](00_meta-framework/archived/INVARIANTS_v3_3.md) — superseded invariant catalog.
- [`LIVENESS_v2.1.md`](00_meta-framework/archived/LIVENESS_v2.1.md) — historical projection/liveness framework.
- [`ADR-resolution-half_v2.md`](00_meta-framework/archived/ADR-resolution-half_v2.md) — historical bindings/obligations/reachability ADR.

## 01 · Substrate

<!-- txdoc:INDEX-SUBSTRATE-1 -->

Primitives that sit below every subsystem.

- [`HAL_v1.md`](01_substrate/HAL_v1.md) — platform abstraction layer: trait crate / board crate split, boot handoff, pmap, traps, cache/DMA/SMP axes.
- [`PAGE_SUBSTRATE_v1.md`](01_substrate/PAGE_SUBSTRATE_v1.md) — physical frame allocator, `FrameMeta`, pmap stages, slab kernel heap.
- [`EBR_ZONE_INTERFACE_v1.md`](01_substrate/EBR_ZONE_INTERFACE_v1.md) — adaptation of EBR and Zone/Cap mechanics to the current `Guard` / `Weak` / `IdentRef` / `Cap` interface, including policy-based-zone boundary decision.
- [`BITMAP_RESERVATION_v1.md`](01_substrate/BITMAP_RESERVATION_v1.md) — `AtomicBitmap<N>` reserve/commit with Drop-rollback.
- [`MUTATION_COMPOSITIONS_v1.md`](01_substrate/MUTATION_COMPOSITIONS_v1.md) — named atomic-ish patterns combining substrate-primitive operations.
- [`BUS_v1.md`](01_substrate/BUS_v1.md) — `RawQueue` / `RawPort` / `RawTrace` publication primitives, wire declaration and subscription.

## 02 · Execution model

<!-- txdoc:INDEX-EXECUTION-1 -->

How work runs: the step primitive and the runtime that drives it.

- [`03_STEP_MODEL_v2.md`](../Txv3/03_STEP_MODEL_v2.md) — the synchronous, bounded step; outcome algebra; five-stage in-step discipline.
- [`THREAD_RUNTIME_v1.md`](02_execution/THREAD_RUNTIME_v1.md) — running-thread states, reactor interaction, signal-delivery boundary.
- [`REACTOR_v0.md`](02_execution/REACTOR_v0.md) — reactor boundary contract.
- [`SCHEDULER_v0.md`](02_execution/SCHEDULER_v0.md) — scheduler policy and interface contract.
- [`COMPLETION_v1.md`](02_execution/COMPLETION_v1.md) — Linux-inspired completion objects as reactor/wait middleware, not bus primitives or semantic truth.
- [`EXEC_v1.md`](02_execution/EXEC_v1.md) — execve script spec: VFS/Mount/Cred/Loader/VM/Process/FD/Signal/ThreadRuntime composition and point-of-no-return discipline.
- [`cred_service_v_1_draft (2).md`](<02_execution/cred_service_v_1_draft (2).md>) — credential service: durable identity-derived policy, authorization checks, credential-changing transitions.
- [`cred_snapshot_wiring_v_1.md`](02_execution/cred_snapshot_wiring_v_1.md) — companion to `cred_service_v_1`: snapshot capture model, `cred::checks::*` witness surface, syscall-arm wiring map, closed-bypass audit, and open audit items as shipped.
- [`rlimit_service_v_1_draft (1).md`](<02_execution/rlimit_service_v_1_draft (1).md>) — rlimit service: per-process resource ceilings, stable usage counters, reservation-backed consumption accounting.

## 03 · Memory / VM

<!-- txdoc:INDEX-MEMORY-VM-1 -->

- [`PAGE_BACKED_v1.md`](03_memory-vm/PAGE_BACKED_v1.md) — `PageContainer`, three-variant `RNodeBacking`; unifies files, tmpfs, shm, memfd, anon mmap, MMIO devices.
- [`VM_v1_2.md`](03_memory-vm/VM_v1_2.md) — `AddressSpace`, `VmEntry`, recipes BTree, `RangeLock`, scripts for mmap/munmap/mprotect/mremap/fault/fork/exec.

## 04 · Process & signals

<!-- txdoc:INDEX-PROCESS-SIGNALS-1 -->

- [`PROCESS_v1.md`](04_process-signals/PROCESS_v1.md) — process subsystem: identity / payload / group / session, fork/clone/exec/exit/wait/setpgid/setsid/kill.
- [`SIGNAL_v1.md`](04_process-signals/SIGNAL_v1.md) — POSIX signal compatibility shim over the native Gewalt-vs-event factoring.
- [`SIGNAL_ATTACHMENTS_v1.md`](04_process-signals/SIGNAL_ATTACHMENTS_v1.md) — per-subsystem publication catalog (which transitions publish on which carriers).

## 05 · Filesystem / VFS

<!-- txdoc:INDEX-FILESYSTEM-1 -->

- [`MOUNT_v1.md`](05_filesystem/MOUNT_v1.md) — mount subsystem: mount namespaces, mount tree, mountpoint index, filesystem-instance hosting, mount/umount steps.
- [`VFS_CHECKS_V2.1.md`](05_filesystem/VFS_CHECKS_V2.1.md) — VFS walker, witness consumption at STEP-4 stage 2, refinement wrappers.
- [`BDEV_FS.md`](05_filesystem/BDEV_FS.md) — block-device pseudo-filesystem; bytes ↔ blocks translation over PAGE_BACKED.
- [`bringup_fs_specs_v_1 (1).md`](<05_filesystem/bringup_fs_specs_v_1 (1).md>) — bringup filesystem specs for tmpfs, initramfs cpio `newc`, and minimal procfs.
- [`TX_EXT4_PLAN_v1_2.md`](05_filesystem/TX_EXT4_PLAN_v1_2.md) — ext4 backend project plan; stateless-per-inode rule, cache & reclaim policy.

## 06 · Devices

<!-- txdoc:INDEX-DEVICES-1 -->

- [`DEVICE.md`](06_devices/DEVICE.md) — three-tier device subsystem; tier 1 in HAL, tier 2 statically composed, tier 3 deferred; four routes to userspace via VFS.
- [`TTY.md`](06_devices/TTY.md) — terminal subsystem; hardware TTY, ptys, line discipline, job control, devpts; `TtyIdentity` / `TtyPayload` / `TtyTransport`.

---

## Reading orders

<!-- txdoc:INDEX-READING-ORDERS-1 -->

**For a new contributor.** 00 active docs → 01 HAL → 01 PAGE_SUBSTRATE → 01 BUS → 02 STEP_MODEL → 02 THREAD_RUNTIME → pick a subsystem (03–06).

**For VM work.** 00 01_CONCEPTS_v5, 02_INVARIANTS_v5, object_model_v2, SUBSYSTEM_ANATOMY_v2_1 → 01 PAGE_SUBSTRATE → 03 PAGE_BACKED → 03 VM.

**For filesystem / driver work.** 00 (all) → 01 HAL → 01 BUS → 03 PAGE_BACKED → 05 MOUNT → 05 VFS_CHECKS → 05 BDEV_FS → 05 bringup_fs_specs → 05 TX_EXT4_PLAN; for char devices add 06 DEVICE → 06 TTY.

**For process / signal work.** 00 (all) → 02 03_STEP_MODEL_v2 → 02 THREAD_RUNTIME → 02 cred_service / rlimit_service → 04 PROCESS → 04 SIGNAL → 04 SIGNAL_ATTACHMENTS → 02 EXEC.
