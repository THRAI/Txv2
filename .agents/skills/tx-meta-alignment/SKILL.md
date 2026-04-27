---
name: tx-meta-alignment
description: Use when editing canonical meta-framework docs: concepts, invariants, object model, subsystem anatomy, module map, or index.
---

# tx-meta-alignment

Use this skill for `docs/design/00_meta-framework/` changes and top-level read-order changes.

## Read First

- `docs/design/00_meta-framework/CONCEPTS_v4.md`
- `docs/design/00_meta-framework/INVARIANTS_v4.md`
- `docs/design/00_meta-framework/object_model_v2.md`
- `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`
- `docs/design/00_meta-framework/MODULE_MAP_v1.md`
- `docs/design/INDEX.md`

## Preserve

- Three basis claims: resolution, lifecycle, publication.
- Current implementation object model is `object_model_v2.md`.
- `INVARIANTS_v4.md` is canonical for enforceable rules.
- `SUBSYSTEM_ANATOMY_v2_1.md` is canonical for subsystem shape and five-phase discipline.
- `FRAMEWORK_HARVEST_v1.md` and `INVARIANT_LEDGER_v1.md` are noncanonical source-trace notes.

## Required Alignment

- New architectural terms need a home in `CONCEPTS_v4.md` and an enforceable rule in `INVARIANTS_v4.md` only when they are load-bearing.
- New subsystem requirements should appear in `SUBSYSTEM_ANATOMY_v2_1.md` or the owning subsystem spec, not only in prose elsewhere.
- Avoid future-doc dependencies unless the future doc is explicitly marked nonblocking.

## Done Means

- `docs/design/INDEX.md` read order still points to current canonical docs.
- Companion lists do not point active docs at archived docs except as source material.
- Stale-doc scans only leave intentional supersession notes.
