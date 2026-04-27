# Framework Harvest — v1

<!-- txdoc:00-META-FRAMEWORK-FRAMEWORK-HARVEST-V1 -->

**Status.** Noncanonical staging note. Superseded for implementation by `CONCEPTS_v4.md`, `INVARIANTS_v4.md`, `MODULE_MAP_v1.md`, `object_model_v2.md`, and `SUBSYSTEM_ANATOMY_v2_1.md`.

**Purpose.** Preserve the harvest trail: stable ideas, promoted subsystem decisions, stale vocabulary, and integration points that shaped the current meta-framework cleanup. This is not a canonical architecture document and should not be used as an implementation contract.

**Working thesis.** The existing meta-framework is mostly directionally right. The rewrite should preserve the stable core, promote decisions that newer subsystem specs have already validated, and remove old scaffolding and broken names before implementation begins.

---

## 1. Target Outcome

<!-- txdoc:FRAMEWORK-HARVEST-TARGET-OUTCOME-1 -->

The rewrite should produce a small canonical meta-framework set:

- `MODULE_MAP_v1.md` — placement taxonomy: foundation, substrate, reactor, scheduler policy, semantic subsystems, service subsystems, filesystem instances, scripts, shims, static registries, projections.
- `CONCEPTS_v4.md` — master vocabulary: three basis claims, planes, roles, reference hierarchy, steps, wait/drive, publication rule, carve-outs.
- `OBJECT_MODEL_v3.md` — entities, identity/payload factoring, references, bindings, obligations, retention, reclamation.
- `INVARIANTS_v4.md` — enforceable rules only; stable identifiers for review/lints/tests.
- `SUBSYSTEM_ANATOMY_v3.md` — shapes for full subsystems, service subsystems, filesystem instances, scripts, shims, static registries, and HAL-facing leaves.
- `PROJECTION_CATALOG_v1.md` — successor to `LIVENESS`: projection rows, partial orders, signal-catalog cross-links, checklist.
- `FRAMEWORK_CHANGELOG.md` — old-to-new map: what superseded `ADR-resolution-half_v2`, `LIVENESS_v2.1`, old filename references, and retired terms.

The canonical docs should be mutually linkable and self-consistent before subsystem specs are mass-edited.

---

## 2. Stable Axioms to Preserve

<!-- txdoc:FRAMEWORK-HARVEST-STABLE-AXIOMS-1 -->

These are load-bearing and should move forward mostly intact.

### 2.1 Three Basis Claims

<!-- txdoc:FRAMEWORK-HARVEST-THREE-BASIS-CLAIMS-1 -->

1. **Resolution.** A kernel maintains `(signifier, consistency, binding)` triples. Userspace names reach entities through binding containers.
2. **Lifecycle.** Every entity decomposes into identity, capability, and payload layers. Identity says "which object"; capability says "which operations"; payload says "which operational resources."
3. **Publication.** Authoritative bindings justify derived materializations. Materialization publication must atomically revalidate the justifying binding.

The third claim should stay a peer, not an appendix. Newer specs use it as the main concurrency lens.

### 2.2 Reference Hierarchy

<!-- txdoc:FRAMEWORK-HARVEST-REFERENCE-HIERARCHY-1 -->

Preserve the four-tier hierarchy:

```text
Weak<T>
  -> IdentRef<'g, T>
  -> Cap<T>
  -> T::OperationalEvidence
```

Rules to keep:

- `IdentRef<'g, T>` is observation only, guard-scoped, and cannot cross steps or yields.
- `Cap<T>` is identity retention and may cross epochs, threads, and awaits.
- operational evidence entails identity retention and pins whatever projection the operation needs.
- upgrades are fallible and are the bridge between EBR traversal and refcounted retention.

### 2.3 Require / Predicate / Witness Discipline

<!-- txdoc:FRAMEWORK-HARVEST-REQUIRE-PREDICATE-WITNESS-1 -->

Keep:

- predicates are pure and guard-scoped;
- `require_*` is the only sanctioned predicate consumer at operation entry;
- witnesses carry `IdentRef` observation evidence, not authority;
- witnesses do not cross guard, thread, async, or step boundaries;
- execution upgrades witnesses before reservation or mutation.

This is still the cleanest anti-TOCTOU story in the corpus.

### 2.4 Step Model

<!-- txdoc:FRAMEWORK-HARVEST-STEP-MODEL-1 -->

Keep:

- steps are synchronous and bounded;
- steps do not await and do not construct futures;
- operations compose steps through drivers and waits;
- progress is monotone;
- `AdvancedThenBlocked` is a first-class outcome;
- witness scope and fresh observation are per-step;
- the operation-level `*_prepare` / `*_commit` pattern remains retired.

The five sub-phases are still good:

```text
observe -> upgrade -> reserve -> commit -> publish
```

The rewrite should be precise about what each sub-phase permits, while avoiding the older phrase "commit phase" for a whole syscall.

### 2.5 Publication Plane

<!-- txdoc:FRAMEWORK-HARVEST-PUBLICATION-PLANE-1 -->

Keep:

- bus signals are not truth;
- wires are not state;
- wake means "re-observe";
- publication is opt-in per transition;
- publication fires after the paired visibility boundary;
- per-carrier ordering only, not global ordering;
- RawQueue / RawPort / RawTrace remain the closed primitive set unless reviewed.

### 2.6 ARCH-5 / Justification Rule

<!-- txdoc:FRAMEWORK-HARVEST-ARCH5-JUSTIFICATION-1 -->

This should become the spine of the rewrite.

The core rule:

> Every derived materialization must be justified by a currently-valid authoritative binding. Publication of the materialization must revalidate that binding atomically with publication.

Preserve the two implementation flavors:

- substrate-linearized conditional commit;
- slot-locked publication with binding re-read.

This now unifies VM PTEs, mount crossing materializations, wait registration, PageContainer population, BDEV coherence, and TTY slot mutation.

---

## 3. Ideas to Promote From Newer Specs

<!-- txdoc:FRAMEWORK-HARVEST-PROMOTED-IDEAS-1 -->

These are not fully captured by the current meta-framework but should become first-class.

### 3.1 Placement Taxonomy

<!-- txdoc:FRAMEWORK-HARVEST-PLACEMENT-TAXONOMY-1 -->

The missing `MODULE_MAP` has become a real blocker. The rewrite needs a canonical taxonomy:

| Category | Meaning | Examples |
|---|---|---|
| Foundation / HAL | Below object model; platform-specific or pre-substrate plumbing | `HAL_v1`, early UART, trap vectors, timer, pmap implementation |
| Substrate | Generic primitive layer with no semantic ownership | zones, indexes, mutation primitives, bus, epoch, pmap substrate, bitmap reservation |
| Reactor | Execution mechanism for tasks, waits, wakeups, preemption, AST carve-outs | `REACTOR_v0` |
| Scheduler policy | Policy consulted by reactor, not a semantic subsystem | `SCHEDULER_v0` |
| Full semantic subsystem | Owns zone-allocated semantic entities and full structure/checks/execution/project shape | Process, VM, Mount, VFS, TTY |
| Service subsystem | Owns durable policy/ledger state but no user-visible namespace of its own | Cred, rlimit, time, trace |
| Filesystem instance | Mounted `FsOps` / `FsPageBacking` backend hosted by Mount | tmpfs, procfs, devfs, devpts, bdev-fs, tx-ext4 |
| Script | Per-syscall composition over checks and steps; owns sequencing, not entities | execve, file I/O scripts, recursive umount |
| Shim | Compatibility surface translating external ABI onto native mechanisms | POSIX signal shim |
| Static registry | Compile-time board/product table outside zone/ref hierarchy | tier-2 device registry |

The taxonomy must say which categories use the four-module layout, which use a reduced form, and which sit outside the object model.

### 3.2 Services Are Not Second-Class

<!-- txdoc:FRAMEWORK-HARVEST-SERVICES-1 -->

Cred and rlimit show that "service subsystem" is not just a footnote.

Promote:

- services may own durable process-attached state;
- services expose pure checks and mutation steps;
- services do not publish foreign subsystem bindings;
- services may produce grants or reservations depending on their model;
- scripts may carry syscall-entry metadata but must not inspect private service state.

Cred model:

- authoritative credential ledger;
- subsystem-local grants are derived materializations minted by owning subsystems;
- authorization is publication justification.

Rlimit model:

- durable ceiling ledger;
- stable usage counters;
- operation-local reservations;
- no reusable grant objects.

### 3.3 Filesystem Instances as Architecture Objects

<!-- txdoc:FRAMEWORK-HARVEST-FILESYSTEM-INSTANCES-1 -->

Mount, bdev-fs, bringup FS, and tx-ext4 all depend on a clearer concept:

> A filesystem instance is not a full semantic subsystem by default. It is a mounted backend object hosted by `MountPayload`, exporting `FsOps` and optionally `FsPageBacking`.

Promote:

- Mount owns topology and filesystem-instance hosting;
- VFS owns DEntry/RNode/OpenFile semantics;
- FS instances own backend-specific object IDs and metadata stores;
- PageBacked owns page-indexed file content behavior;
- procfs/devfs/devpts are filesystem instances, not ad hoc projections bolted onto VFS.

### 3.4 Mount Topology as Authoritative Binding Case Study

<!-- txdoc:FRAMEWORK-HARVEST-MOUNT-TOPOLOGY-1 -->

Mount has sharpened the publication model:

- the mountpoint index is the authoritative crossing binding;
- `MountIdentity.mountpoint`, parent DLLs, and `all_mounts` are consistency/materialization fields as specified by mount;
- walkers must revalidate crossing against the mountpoint index;
- lazy umount detaches topology while payload may remain pinned by cwd/root/open-file users.

This should appear as the canonical non-VM ARCH-5 example in `CONCEPTS_v4` or `OBJECT_MODEL_v3`.

### 3.5 Closed Payload-Pin Accounting

<!-- txdoc:FRAMEWORK-HARVEST-PAYLOAD-PIN-ACCOUNTING-1 -->

Mount introduces a useful pattern:

> If a payload may survive topology detachment, the owning subsystem must define a closed catalog of payload-pin acquirers.

This generalizes to:

- unlinked files;
- detached mounts;
- TTYs/ptys;
- page-cache / pmap frames;
- process payload vs zombie identity.

The rewrite should distinguish generic operational evidence from closed subsystem-specific pin catalogs.

### 3.6 Scripts With Point of No Return

<!-- txdoc:FRAMEWORK-HARVEST-SCRIPT-POINT-OF-NO-RETURN-1 -->

Exec makes a script-level concept explicit:

- some scripts have a point of no return;
- all fallible work must be placed before that boundary;
- after the boundary, failures are fatal, deferred, or reported as partial progress depending on syscall semantics;
- PONR is not a new framework phase, but it is a required script design annotation for operations like exec.

Promote PONR into `SUBSYSTEM_ANATOMY_v3` under script design rules.

### 3.7 HAL and Static Devices Outside the Object Model

<!-- txdoc:FRAMEWORK-HARVEST-HAL-STATIC-DEVICES-1 -->

HAL and tier-2 devices show two carve-outs:

- HAL tier-1 devices are below the object model and have no devfs presence.
- tier-2 static devices may be `&'static` table entries, not zone-allocated entities; they sit outside `Weak/IdentRef/Cap`.

The framework should explicitly allow this without forcing fake entities into the hierarchy.

### 3.8 Scheduler as Policy, Reactor as Mechanism

<!-- txdoc:FRAMEWORK-HARVEST-SCHEDULER-REACTOR-1 -->

The current framework says reactor is infrastructure, but scheduler needs clearer placement:

- reactor owns polling, wake, preemption mechanism, task futures, AST slots;
- scheduler owns selection policy and per-task scheduling metadata;
- scheduler policy is not a full semantic subsystem and does not own the execution mechanism.

### 3.9 POSIX Shim Layer

<!-- txdoc:FRAMEWORK-HARVEST-POSIX-SHIM-1 -->

Signal distinguishes native mechanisms from ABI compatibility:

- native Gewalt/control operations target process/thread runtime mechanisms directly;
- event signals are bus publications and queues;
- POSIX signal semantics are a shim over native process/thread/bus/HAL behavior.

The framework should name "shim" as a category so POSIX, Linux ABI, and future compatibility surfaces do not masquerade as core subsystems.

---

## 4. Terms to Keep, Rename, or Retire

<!-- txdoc:FRAMEWORK-HARVEST-TERMS-1 -->

### 4.1 Keep

<!-- txdoc:FRAMEWORK-HARVEST-TERMS-KEEP-1 -->

- authoritative binding
- derived materialization
- justification invariant
- publication rule
- visibility boundary
- commit point
- projection
- witness
- obligation
- operational evidence
- service subsystem
- filesystem instance
- script
- foundation / HAL

### 4.2 Rename / Normalize

<!-- txdoc:FRAMEWORK-HARVEST-TERMS-RENAME-1 -->

| Current / stale | New canonical form |
|---|---|
| `object_model_0417.md` | `OBJECT_MODEL_v3.md` |
| `object_model_v2.md` | superseded by `OBJECT_MODEL_v3.md` |
| `CONCEPTS.md` | `CONCEPTS_v4.md` |
| `INVARIANTS.md` | `INVARIANTS_v4.md` |
| `SUBSYSTEM_ANATOMY.md` | `SUBSYSTEM_ANATOMY_v3.md` |
| `LIVENESS.md` | `PROJECTION_CATALOG_v1.md` or `LIVENESS` only as historical term |
| dispatch class | script-phase class |
| pipeline | script |
| FileOps / InodeOps vtable | `FsOps` on filesystem instance; PageBacked for content |
| `sig/` subsystem | process-owned signal structures plus POSIX signal shim |

### 4.3 Retire

<!-- txdoc:FRAMEWORK-HARVEST-TERMS-RETIRE-1 -->

- `ADR-resolution-half_v2` as canonical spec. Keep only as historical background or fold into changelog.
- `MODULE_MAP` references without an actual module-map document.
- "forthcoming" labels for docs that now exist: BUS, SIGNAL_ATTACHMENTS, REACTOR, VM, PAGE_BACKED.
- same-folder relative links in moved docs when target lives in another group.
- operation-level prepare/commit as architecture vocabulary.

---

## 5. Known Contradictions and Integration Gaps

<!-- txdoc:FRAMEWORK-HARVEST-CONTRADICTIONS-GAPS-1 -->

### 5.1 Object Model Still Says Two Claims

<!-- txdoc:FRAMEWORK-HARVEST-TWO-CLAIMS-GAP-1 -->

`object_model_v2` still opens with the two-claim v11 basis. The new canonical object model should instead present itself as the lifecycle/resolution half under the three-claim framework, with publication as a peer handled across object and substrate rules.

### 5.2 Liveness Is Both Theory and Catalog

<!-- txdoc:FRAMEWORK-HARVEST-LIVENESS-GAP-1 -->

`LIVENESS_v2.1` mixes:

- projection theory;
- require/witness discipline;
- per-subsystem projection catalog;
- reclamation rules;
- checklist.

The theory now belongs in `CONCEPTS_v4`, `OBJECT_MODEL_v3`, and `INVARIANTS_v4`. The catalog/checklist should become `PROJECTION_CATALOG_v1`.

### 5.3 Subsystem Anatomy Understates Non-Full Shapes

<!-- txdoc:FRAMEWORK-HARVEST-NON-FULL-SHAPES-GAP-1 -->

It has a service subsystem variant, but newer docs require more:

- reduced services with policy ledgers;
- filesystem instances;
- static registries;
- shims;
- script-only specs with no entities;
- HAL-facing leaves.

`SUBSYSTEM_ANATOMY_v3` should be a family of shapes, not a single shape plus footnotes.

### 5.4 Cross-Doc Edits Are Accumulating

<!-- txdoc:FRAMEWORK-HARVEST-CROSS-DOC-EDITS-GAP-1 -->

Docs with explicit pending edits:

- `EXEC_v1`: process payload fields, VM detached AS APIs, PageBacked `read_exact_at`, signal attachment row, cred reservation lane.
- `MOUNT_v1`: Process fs-context mount fields, VFS walker mount fields, OpenFile mount pins, PageBacked mount back-reference wording, signal attachment wording.
- `bringup_fs_specs_v_1`: VFS/PageBacked/Mount integration points for tmpfs/procfs/initramfs.
- `PROCESS_v1`, `THREAD_RUNTIME_v1`, `SCHEDULER_v0`: corrections around signal mask, thread runtime, scheduler boundaries.

The rewrite should collect these into a single "integration ledger" before mass-editing subsystem docs.

### 5.5 Filename Drift Is Severe

<!-- txdoc:FRAMEWORK-HARVEST-FILENAME-DRIFT-1 -->

A link scan found hundreds of stale links because docs were reorganized into numbered folders and versioned filenames. This is mechanical rot, but it creates conceptual friction.

Canonical rewrite should choose link style:

- root-relative repo paths in prose, or
- stable unversioned symlink/alias docs, or
- versioned filenames but all links updated.

Do not leave a mix.

---

## 6. Proposed Rewrite Sequence

<!-- txdoc:FRAMEWORK-HARVEST-REWRITE-SEQUENCE-1 -->

### Phase A — Harvest and Agreement

<!-- txdoc:FRAMEWORK-HARVEST-PHASE-A-1 -->

1. Review this harvest.
2. Add missing stable ideas and promoted ideas.
3. Mark any disputed items.
4. Decide final canonical doc names.

Exit criterion: no major taxonomy dispute remains.

### Phase B — New Canonical Skeleton

<!-- txdoc:FRAMEWORK-HARVEST-PHASE-B-1 -->

Write skeletons in this order:

1. `MODULE_MAP_v1.md`
2. `CONCEPTS_v4.md`
3. `OBJECT_MODEL_v3.md`
4. `INVARIANTS_v4.md`
5. `SUBSYSTEM_ANATOMY_v3.md`
6. `PROJECTION_CATALOG_v1.md`

Exit criterion: every canonical doc has scope, companion docs, and empty section headings.

### Phase C — Populate the Spine

<!-- txdoc:FRAMEWORK-HARVEST-PHASE-C-1 -->

Populate:

1. taxonomy and placement rules;
2. three basis claims and planes;
3. reference/binding/obligation model;
4. ARCH-5 publication rule;
5. step/wait/script model;
6. subsystem/service/script/FS-instance shapes.

Exit criterion: a new subsystem author can classify a mechanism without opening old docs.

### Phase D — Integrate Catalogs

<!-- txdoc:FRAMEWORK-HARVEST-PHASE-D-1 -->

Populate:

- projection catalog;
- signal attachment pointers;
- closed catalogs;
- carve-outs;
- import/lint rules.

Exit criterion: review/lint vocabulary is stable.

### Phase E — Subsystem Reconciliation

<!-- txdoc:FRAMEWORK-HARVEST-PHASE-E-1 -->

Apply cross-doc edits:

- Exec corrections;
- Mount corrections;
- Bringup FS corrections;
- Process/thread/scheduler/signal corrections;
- filesystem instance placement;
- HAL/static device carve-outs.

Exit criterion: subsystem docs cite new canonical docs and no longer cite missing files.

### Phase F — Mechanical Cleanup

<!-- txdoc:FRAMEWORK-HARVEST-PHASE-F-1 -->

1. Update all links.
2. Update `INDEX.md`.
3. Mark retired docs clearly.
4. Run link check.

Exit criterion: no broken Markdown links except explicitly external or intentionally missing future specs.

---

## 7. First Draft of the New Architecture Story

<!-- txdoc:FRAMEWORK-HARVEST-ARCHITECTURE-STORY-1 -->

txKernel is a constrained transition system.

Userspace names enter through signifiers. Signifiers resolve through authoritative binding containers under epoch guards. Checks turn current predicate truth into guard-scoped witnesses. Steps consume witnesses, upgrade them into retention, reserve private resources, commit observer-visible transitions at visibility boundaries, and publish selected hints on bus wires. Scripts compose these steps into syscalls, using the reactor to wait and retry; scripts do not inspect subsystem-internal state. Semantic subsystems own truth. Service subsystems own durable policy ledgers or accounting state. Filesystem instances are hosted by Mount and implement backend object operations. HAL and selected static registries sit outside the object model where forcing zone-allocated identity would be false structure.

All cached or pre-materialized state is subordinate to authoritative bindings. Publication revalidates the binding that justifies the materialization. Withdrawal either invalidates dependents or makes future publication against the withdrawn binding fail. Races degrade to clean failure, never silent retargeting.

That is the spine. The rewrite should keep every canonical document attached to it.

---

## 8. Open Questions for the Rewrite

<!-- txdoc:FRAMEWORK-HARVEST-OPEN-QUESTIONS-1 -->

1. Should `LIVENESS` disappear as a name, or remain as a short conceptual alias for `PROJECTION_CATALOG`?
2. Should canonical filenames be versioned (`CONCEPTS_v4.md`) or stable (`CONCEPTS.md`) with version headers only?
3. Should `SIGNAL_ATTACHMENTS_v1.md` stay under `04_process-signals`, or move into meta-framework as a cross-subsystem catalog?
4. Should `BUS_v1.md` remain under substrate, or should meta-framework own a short bus concept doc and leave APIs in substrate?
5. How much of `ADR-resolution-half_v2` should be preserved as historical rationale versus deleted after fold-in?
6. Should PONR be an invariant class, or just a script-design annotation?
7. Do static device registries need a formal "static identity" concept, or is "outside the reference hierarchy" enough?
