---
name: tx-subsystem-manifest
description: Use when adding or auditing subsystem docs, especially zone-derived type policy tables and entity/reference classification.
---

# tx-subsystem-manifest

Use this skill for subsystem docs under execution, VM, process/signals, filesystem, and devices.

## Read First

- `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`
- `docs/design/00_meta-framework/object_model_v2.md`
- `docs/Txv3/02_INVARIANTS_v5.md` — canonical for STEP-* (updated for four-variant outcome) and the new SUBJ-*, YIELD-*, DELEGATE-*, SCOPE-* families.
- `docs/design/00_meta-framework/INVARIANTS_v4.md` — ZONE, BIF, OBL, WIT rows are carried forward unchanged; cite v4 anchors when restating them since other subsystem docs do.
- `docs/Txv3/03_STEP_MODEL_v2.md` — current step algebra; supersedes `02_execution/STEP_MODEL_v1.md`.
- `docs/Txv3/04_SYSCALL_SHAPE_v1.md` — upper/lower split discipline if your subsystem owns syscall entry points.
- The owning subsystem spec

## Required Section

Subsystem specs that own or consume reclaimable semantic entities should include:

```md
### Zone-derived type policy

| Declaration | Public handle | Reclamation role |
|---|---|---|
| ... | ... | ... |
```

## Classify Each Thing

- semantic entity: co-located, identity/payload split, identity-only, or compound-payload.
- stored reference: resolution-only, addressability, operational, witness, or projection.
- container value: binding value, not a zone entity.
- platform/device fact: static fact, not a zone entity.
- script/shim/reaction mechanism: composer or adapter, not owner.

## Done Means

- The subsystem manifest says what owns each entity.
- Binding obligations derive stored evidence.
- Witnesses carry `IdentRef<'g, T>` and do not escape.
- Static facts are not wrapped in caps.
- Derived materializations name their authoritative binding.
