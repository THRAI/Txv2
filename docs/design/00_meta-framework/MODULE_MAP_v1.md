# Module Map — v1

<!-- txdoc:00-META-FRAMEWORK-MODULE-MAP-V1 -->

**Status.** Draft v1 for meta-framework unification.

**Purpose.** Define where txKernel mechanisms live. This document classifies code and specs by architectural role so subsystem authors can decide whether a mechanism is foundation, substrate, reactor, policy, semantic subsystem, service subsystem, filesystem instance, script, shim, projection, or static registry before writing code.

**Audience.** Designers, reviewers, and implementation agents. Use this before adding a module, moving a spec, or deciding which layer owns a behavior.

**Companion documents.**

- [`FRAMEWORK_HARVEST_v1.md`](FRAMEWORK_HARVEST_v1.md) — staging notes that motivated this map.
- [`CONCEPTS_v4.md`](CONCEPTS_v4.md) — unified concepts document.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](SUBSYSTEM_ANATOMY_v2_1.md) — current full-subsystem shape; to be generalized by `SUBSYSTEM_ANATOMY_v3.md`.
- [`INVARIANTS_v4.md`](INVARIANTS_v4.md) — canonical invariant set and linter labels.

---

## 1. Placement Rule

<!-- txdoc:MODULE-MAP-PLACEMENT-RULE-1 -->

Every mechanism must have one primary architectural home:

| Home | Owns | Does not own |
|---|---|---|
| Foundation / HAL | platform-specific boot, traps, low-level hardware facts | semantic objects, syscall policy |
| Substrate | generic allocation, indexing, mutation, publication, epoch, pmap primitives | kernel semantics |
| Reactor | task polling, waits, wake delivery, preemption mechanism, AST slots | scheduling policy, semantic state |
| Scheduler policy | task selection and budget policy | polling mechanism, semantic objects |
| Full semantic subsystem | user-visible or kernel-semantic entities and transitions | cross-syscall sequencing |
| Service subsystem | durable policy/ledger state used by other owners | foreign bindings or namespace ownership |
| Filesystem instance | mounted backend object model and backend operations | VFS graph ownership or mount topology |
| Script | syscall sequencing over checks and steps | durable entities or authoritative indexes |
| Shim | compatibility ABI translation over native mechanisms | native truth if another owner already has it |
| Projection | read-only rendering of owner state | cached truth, mutation, authorization |
| Static registry | compile-time tables outside zone identity | dynamic lifetime unless promoted to subsystem |

If a mechanism seems to need two homes, split it into an owner and a consumer. If it seems to need a new home, first try to express it as a variant of one of the categories above; adding a category is an architecture change.

Full semantic subsystems that own reclaimable entities use policy-based zones
as their common lifetime substrate. The subsystem's public surface remains
role-shaped (`Cap<T>`, `PayloadCap<T>`, `Weak<T>`, witnesses, identity slots,
projection rows); raw zone policy parameters belong to substrate internals or
entity-zone declarations, not operation modules.

---

## 2. Directory Taxonomy

<!-- txdoc:MODULE-MAP-DIRECTORY-TAXONOMY-1 -->

The eventual implementation layout should reflect this map. Exact crate names may differ, but ownership boundaries should not.

```text
foundation/
    hal/                  platform traits, board crates, trap/pmap/timer hooks

substrate/
    zone/                 zone allocation and signing
    index/                key reservation and commit
    mutation/             withdraw, swap, install-if, structural compositions
    credit/               resource reservations
    bitmap/               bitmap reservations
    epoch/                EBR guards and retire queues
    bus/                  RawQueue, RawPort, RawTrace
    pmap/                 generic pmap-facing substrate traits/helpers
    shootdown/            synchronous TLB coordination shell

reactor/
    task/                 task futures and poll boundaries
    wait/                 wait primitive and wake integration
    ast/                  async trap/event delivery slots
    preempt/              userspace preemption mechanism

policy/
    scheduler/            scheduler policy trait and implementations
    cgroup/               future resource policy hierarchy

subsystems/
    process/
    thread_runtime/
    vm/
    page_backed/
    vfs/
    mount/
    tty/
    ...

services/
    cred/
    rlimit/
    time/
    trace/
    random/

fs/
    tmpfs/
    procfs/
    devfs/
    devpts/
    bdevfs/
    tx_ext4/

scripts/
    route/
    prelude/
    postlude/
    process/exec.rs
    file_io.rs
    mount.rs
    ...

shims/
    posix_signal/
    linux_syscall/
    ...

static_registry/
    devices/
    platform_tables/
```

The map is conceptual. A small kernel may collapse directories physically, but imports and ownership must still follow the categories.

---

## 3. Foundation / HAL

<!-- txdoc:MODULE-MAP-FOUNDATION-HAL-1 -->

Foundation/HAL sits below the object model.

Examples:

- firmware entry and boot handoff;
- board selection;
- trap vectors and trap-frame views;
- early console;
- IRQ controller and timer source;
- concrete pmap implementation;
- cache/DMA fences;
- low-level SMP mechanics;
- architecture-specific signal-frame register layout.

Rules:

- HAL is an axHal-style static platform family, not a runtime manager or semantic subsystem.
- Platform crates own `_start`, linker placement, raw firmware registers,
  early console, shutdown, and `BootPlatformIf`; board binaries only join one
  concrete platform to the generic kernel through `rust_entry`.
- HAL does not own zone-allocated semantic entities.
- HAL must not depend on semantic subsystems.
- Generic kernel code depends on HAL traits and typed `BootHandoff`, not board
  crates, firmware registers, or runtime architecture switches.
- Board binary crates are the only place where concrete platform and generic kernel are joined.
- Tier-1 devices used only for boot/trap/timer/early console live here and do not appear in devfs.

Relevant specs: [`HAL_v1.md`](../01_substrate/HAL_v1.md), [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md), [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md).

---

## 4. Substrate

<!-- txdoc:MODULE-MAP-SUBSTRATE-1 -->

Substrate provides semantic-free primitives. It knows about atomicity, retention, keys, reservations, publication carriers, and epochs; it does not know what a process, file, mount, or signal means.

Substrate owns:

- zone reservation/signing;
- index reservation/commit;
- observer-safe mutation primitives;
- structural mutation compositions;
- credit reservations;
- bitmap reservations;
- epoch guards and reclaim queues;
- bus primitives;
- pmap-facing primitive surfaces;
- shootdown coordination shell.

Rules:

- Substrate primitives are named by behavior, not by subsystem semantics.
- Substrate reservations are linear and roll back on drop.
- Substrate commit primitives are visibility boundaries.
- Bus fire APIs are called from semantic commit/publish code, not from checks.
- Substrate does not decide errno, policy, or user-visible meaning.

Relevant specs: [`BITMAP_RESERVATION_v1.md`](../01_substrate/BITMAP_RESERVATION_v1.md), [`BUS_v1.md`](../01_substrate/BUS_v1.md), [`MUTATION_COMPOSITIONS_v1.md`](../01_substrate/MUTATION_COMPOSITIONS_v1.md), [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md).

---

## 5. Reactor

<!-- txdoc:MODULE-MAP-REACTOR-1 -->

The reactor owns execution mechanism.

It owns:

- task submission and polling;
- wait registration and wake delivery;
- interruptible/killable wait integration;
- userspace preemption mechanism;
- AST slots and return-to-user delivery boundary;
- cross-core synchronous coordination shell when specified by reactor/shootdown contracts.

It does not own:

- scheduling policy;
- semantic entity state;
- syscall admission policy;
- POSIX signal semantics.

Rules:

- A step never calls into the executor to await.
- Drivers/scripts interpret `StepOutcome` and invoke reactor waits.
- Wake delivery never grants truth; scripts re-enter steps/checks.
- AST is a reactor carve-out consumed by thread runtime and signal delivery.

Relevant specs: [`REACTOR_v0.md`](../02_execution/REACTOR_v0.md), [`THREAD_RUNTIME_v1.md`](../02_execution/THREAD_RUNTIME_v1.md), [`STEP_MODEL_v1.md`](../02_execution/STEP_MODEL_v1.md).

---

## 6. Scheduler Policy

<!-- txdoc:MODULE-MAP-SCHEDULER-POLICY-1 -->

Scheduler policy is consulted by the reactor. It is not the reactor and not a full semantic subsystem.

It owns:

- run-queue policy;
- task class and priority policy;
- time-slice budget decisions;
- wake placement hints;
- future fair-share / RT / cgroup CPU policy.

It does not own:

- task polling;
- trap/preemption mechanics;
- process lifecycle;
- wait queues;
- user-visible process semantics.

Rules:

- Scheduler policy may store scheduler metadata attached to tasks.
- Reactor invokes scheduler policy at dispatch and stop points.
- Full fair-share and RT semantics may extend policy without changing reactor ownership.

Relevant spec: [`SCHEDULER_v0.md`](../02_execution/SCHEDULER_v0.md).

---

## 7. Full Semantic Subsystems

<!-- txdoc:MODULE-MAP-FULL-SEMANTIC-SUBSYSTEMS-1 -->

A full semantic subsystem owns kernel truth for a domain. It usually owns zone-allocated entities and uses the full shape:

```text
subsystem/
    structure/
    checks/
    execution/
    project.rs
```

It owns:

- semantic entities;
- authoritative indexes and bindings;
- predicates and witnesses;
- mutating step functions;
- projection rendering for its own state;
- signal attachment declarations for its transitions.

Rules:

- `structure/` is the source of truth and is written only by `execution/` commit code.
- `checks/` is pure and produces witnesses.
- `execution/` consumes witnesses, upgrades, reserves, commits, and publishes.
- `project.rs` is read-only and never authorizes.
- full subsystems may expose narrow APIs consumed by scripts, services, or filesystem instances.

Examples:

- Process owns process/thread/group/session lifecycle and process-attached policy slots.
- VM owns address spaces, recipes, range coordination, and PTE materialization rules.
- PageBacked owns PageContainer and page-indexed content behavior.
- VFS owns DEntry, RNode, OpenFile, fd-facing filesystem object semantics.
- Mount owns mount topology and filesystem-instance hosting.
- TTY owns dynamic terminal entities and line discipline.

Relevant specs: [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md), [`VM_v1_2.md`](../03_memory-vm/VM_v1_2.md), [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md), [`VFS_CHECKS_V2.1.md`](../05_filesystem/VFS_CHECKS_V2.1.md), [`MOUNT_v1.md`](../05_filesystem/MOUNT_v1.md), [`TTY.md`](../06_devices/TTY.md).

---

## 8. Service Subsystems

<!-- txdoc:MODULE-MAP-SERVICE-SUBSYSTEMS-1 -->

Service subsystems own durable policy, ledger, or helper state, but they do not own a user-visible namespace and do not publish foreign subsystem objects.

Reduced shape:

```text
service/
    structure/     durable service-owned state
    checks/        pure policy checks
    execution/     service-owned mutations
    project.rs     optional read-only projections
```

Rules:

- Services authorize or account; owning subsystems publish.
- Services may expose pure checks over service state and caller metadata.
- Services may expose mutation steps for their own state.
- Services do not install fd entries, mount bindings, VM mappings, process links, or VFS nodes on behalf of owners.
- Scripts may pass service outputs to owning subsystems but must not inspect private service structure.

Cred service:

- owns durable credential state;
- exposes authorization checks;
- supports credential-changing transitions;
- justifies subsystem-local grants minted by the owning subsystem.

Rlimit service:

- owns durable ceilings and usage counters;
- exposes admissibility checks;
- cooperates with substrate reservations;
- does not mint reusable authority.

Relevant specs: [`cred_service_v_1_draft (2).md`](<../02_execution/cred_service_v_1_draft (2).md>), [`rlimit_service_v_1_draft (1).md`](<../02_execution/rlimit_service_v_1_draft (1).md>).

---

## 9. Filesystem Instances

<!-- txdoc:MODULE-MAP-FILESYSTEM-INSTANCES-1 -->

A filesystem instance is a backend hosted by Mount and consumed by VFS/PageBacked. It is not automatically a full semantic subsystem.

It owns:

- backend-specific object IDs;
- backend metadata store;
- `FsOps`;
- optional `FsPageBacking`;
- backend-specific mount payload state.

It does not own:

- mount topology;
- DEntry/RNode/OpenFile lifecycle;
- fd-table semantics;
- VM PTEs;
- process state.

Rules:

- Mount stores the filesystem-instance trait objects on `MountPayload`.
- VFS calls `FsOps` to resolve or mutate backend objects.
- PageBacked calls `FsPageBacking` for page fetch/flush.
- Backend-local caches must either be derived materializations or explicitly named authoritative bindings.
- Synthetic filesystems such as procfs and devfs are still filesystem instances when they mount and answer `FsOps`.

Examples:

- tmpfs;
- initramfs unpack target;
- procfs;
- devfs;
- devpts;
- bdev-fs;
- tx-ext4.

Relevant specs: [`MOUNT_v1.md`](../05_filesystem/MOUNT_v1.md), [`BDEV_FS.md`](../05_filesystem/BDEV_FS.md), [`TX_EXT4_PLAN_v1_2.md`](../05_filesystem/TX_EXT4_PLAN_v1_2.md), [`bringup_fs_specs_v_1 (1).md`](<../05_filesystem/bringup_fs_specs_v_1 (1).md>), [`DEVICE.md`](../06_devices/DEVICE.md), [`TTY.md`](../06_devices/TTY.md).

---

## 10. Scripts

<!-- txdoc:MODULE-MAP-SCRIPTS-1 -->

Scripts are per-syscall programs. They compose signifier resolution, service checks, semantic steps, waits, and cross-subsystem sequencing.

Scripts own:

- syscall prelude/postlude sequencing;
- cross-subsystem operation order;
- driver mode selection: nonblocking, waiting, selecting;
- point-of-no-return placement;
- accumulated progress translation to POSIX results.

Scripts do not own:

- semantic entities;
- authoritative bindings;
- subsystem-private structure;
- signal carriers;
- policy ledgers except through service APIs.

Rules:

- Scripts import subsystem `checks/` and `execution/`, not `structure/`.
- Scripts may hold `'static` retention evidence between steps, but not witnesses.
- Scripts re-run checks after waits.
- Scripts may be single-subsystem or cross-subsystem; both are scripts.
- A script with a point of no return must mark the boundary and place fallible work before it.
- Script state is sequencing state, not semantic truth.

Examples:

- `execve` script;
- file I/O scripts spanning VFS/PageBacked/VM;
- recursive umount composition;
- fork/clone composition when it spans process, VM, fd, and signal action state.

Relevant specs: [`EXEC_v1.md`](../02_execution/EXEC_v1.md), [`STEP_MODEL_v1.md`](../02_execution/STEP_MODEL_v1.md), [`SUBSYSTEM_ANATOMY_v2_1.md`](SUBSYSTEM_ANATOMY_v2_1.md).

---

## 11. Shims

<!-- txdoc:MODULE-MAP-SHIMS-1 -->

Shims translate compatibility ABIs onto native mechanisms.

They own:

- ABI-specific input/output translation;
- compatibility policy where no native owner exists;
- mapping external names to native operations.

They do not own:

- native semantic truth if a subsystem already owns it;
- low-level HAL mechanics;
- bus truth;
- process/thread lifecycle.

Rules:

- A shim must identify the native mechanisms it composes.
- A shim must not duplicate authoritative state owned elsewhere.
- A shim may own ABI tables when those tables are themselves the compatibility truth.

Example:

- POSIX signal compatibility shim: translates POSIX signal semantics onto process/thread runtime, bus events, HAL frame installation, and scheduler/reactor interruption boundaries.

Relevant spec: [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md).

---

## 12. Projections

<!-- txdoc:MODULE-MAP-PROJECTIONS-1 -->

Projection code is read-only rendering of owner state.

It owns:

- formatting;
- snapshot traversal;
- synthetic file contents when mounted through procfs/sysfs-like filesystems;
- read-only views.

It does not own:

- truth separate from owner state;
- mutation;
- authorization;
- cached shadow tables.

Rules:

- Projections read owner structure under the owner-approved observation discipline.
- Procfs/devfs may expose projections, but they must not become separate truth stores.
- Projection output may be stale immediately after rendering; consumers must re-check for action.
- Projection filesystems are filesystem instances; projection functions remain owned by the semantic subsystem whose state is rendered.

Relevant specs: [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md), [`bringup_fs_specs_v_1 (1).md`](<../05_filesystem/bringup_fs_specs_v_1 (1).md>), [`MOUNT_v1.md`](../05_filesystem/MOUNT_v1.md).

---

## 13. Static Registries

<!-- txdoc:MODULE-MAP-STATIC-REGISTRIES-1 -->

Static registries are compile-time tables of objects that are not zone-allocated and not reclaimed through the object model.

Examples:

- tier-2 device registry;
- board-specific driver table;
- static trap-cause dispatch tables;
- static platform metadata tables.

Rules:

- Static entries use `&'static` identity, not `Cap<T>`.
- Static registries must not pretend to participate in epoch/refcount reclamation.
- If an entry needs dynamic lifetime, hotplug, detach, or per-instance payload reclamation, promote it to a full subsystem or a filesystem instance with owned state.
- Static registry entries may be projected into VFS through a filesystem instance such as devfs, but the registry remains the source of the static facts.

Relevant specs: [`DEVICE.md`](../06_devices/DEVICE.md), [`HAL_v1.md`](../01_substrate/HAL_v1.md).

---

## 14. Import Boundary Summary

<!-- txdoc:MODULE-MAP-IMPORT-BOUNDARY-SUMMARY-1 -->

Allowed imports by category:

| From / To | May import |
|---|---|
| HAL | meta vocabulary and primitive types only; no semantic subsystems |
| Substrate | primitive types, atomics, HAL traits where required |
| Reactor | substrate bus/wait/epoch primitives, scheduler policy trait, task metadata |
| Scheduler policy | reactor-facing task metadata and policy inputs |
| Full subsystem `checks/` | own `structure/`, passive substrate observation helpers, other subsystems' public witness/check APIs |
| Full subsystem `execution/` | own `structure/`, own/peer checks, substrate reserve/commit/fire primitives |
| Service `checks/` | service structure, caller metadata, peer value types |
| Service `execution/` | service structure, substrate reservations needed for service-owned state |
| Filesystem instance | Mount/VFS/PageBacked public traits and backend-owned structure |
| Script | public `checks/` and `execution/`, service APIs, reactor wait/drive helpers |
| Shim | public script/subsystem/service APIs and ABI tables |
| Projection | owner-approved read APIs and formatting helpers |

Forbidden imports:

- scripts importing subsystem `structure/`;
- checks importing execution or mutating substrate APIs;
- HAL importing semantic subsystems;
- services publishing foreign subsystem objects;
- filesystem instances mutating VFS or Mount internals directly;
- projections serving as authorization checks;
- static registries using fake `Cap<T>` wrappers for immortal entries.

---

## 15. Document Placement

<!-- txdoc:MODULE-MAP-DOCUMENT-PLACEMENT-1 -->

Suggested spec placement:

| Folder | Contents |
|---|---|
| `docs/design/00_meta-framework/` | canonical framework, object model, invariants, module map, projection catalog |
| `docs/design/01_substrate/` | HAL-facing substrate specs and generic primitives |
| `docs/design/02_execution/` | reactor, scheduler, step model, script specs, service specs if process-attached |
| `docs/design/03_memory-vm/` | VM and PageBacked |
| `docs/design/04_process-signals/` | process, thread/process signal model, POSIX signal shim, signal attachment catalog |
| `docs/design/05_filesystem/` | mount, VFS, filesystem instances, filesystem backend plans |
| `docs/design/06_devices/` | device registry, TTY, devfs/devpts-facing device specs |

This folder layout is documentation layout, not necessarily final crate layout. The ownership categories above are the real rule.

---

## 16. Classification Checklist

<!-- txdoc:MODULE-MAP-CLASSIFICATION-CHECKLIST-1 -->

Before adding a mechanism, answer:

1. What category owns this mechanism?
2. What authoritative binding or ledger is the source of truth?
3. Is any cached state a derived materialization? What binding justifies it?
4. Does it need a zone-allocated identity, or is it static/HAL/foundation?
5. Does it mutate truth, or only render/project it?
6. Does it authorize/account, or publish an owned subsystem object?
7. Is it syscall sequencing rather than semantic ownership?
8. Which imports are required, and are any forbidden by this map?
9. Which document should own the canonical description?
10. Which existing projection and signal catalogs must be updated?
