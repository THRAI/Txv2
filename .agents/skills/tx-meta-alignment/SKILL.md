---
name: tx-meta-alignment
description: "Use when editing canonical meta-framework docs: concepts, invariants, object model, subsystem anatomy, module map, or index."
---

# tx-meta-alignment

Use this skill for `docs/design/00_meta-framework/` changes and top-level read-order changes.

## Read First

- `docs/Txv3/INDEX.md` — the v3 spine; declares which v4 docs are superseded.
- `docs/Txv3/01_CONCEPTS_v5.md` — supersedes `CONCEPTS_v4.md` for new prose.
- `docs/Txv3/02_INVARIANTS_v5.md` — canonical invariant catalog (SUBJ-*, YIELD-*, DELEGATE-*, SCOPE-* families; updated STEP-*).
- `docs/Txv3/03_STEP_MODEL_v2.md` — supersedes `STEP_MODEL_v1.md`.
- `docs/design/00_meta-framework/CONCEPTS_v4.md` — still cited by subsystem docs; v5 carries forward v4 sections it doesn't touch.
- `docs/design/00_meta-framework/INVARIANTS_v4.md` — section anchors that other v4 subsystem docs still cite.
- `docs/design/00_meta-framework/object_model_v2.md`
- `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`
- `docs/design/00_meta-framework/MODULE_MAP_v1.md`
- `docs/design/INDEX.md`

## Preserve

- Three basis claims: resolution, lifecycle, publication.
- Current implementation object model is `object_model_v2.md`.
- `Txv3/02_INVARIANTS_v5.md` is canonical for enforceable rules; `INVARIANTS_v4.md` is the prior catalog whose section anchors are still referenced.
- `SUBSYSTEM_ANATOMY_v2_1.md` is canonical for subsystem shape and five-phase discipline.
- `FRAMEWORK_HARVEST_v1.md` and `INVARIANT_LEDGER_v1.md` are noncanonical source-trace notes.

## Required Alignment

- New architectural terms belong in `Txv3/01_CONCEPTS_v5.md` (or, if the term is load-bearing for a v4 subsystem doc that hasn't been touched, in `CONCEPTS_v4.md` with a v5 cross-reference).
- New enforceable rules belong in `Txv3/02_INVARIANTS_v5.md`. Cite v4 only when restating a row v5 carries forward unchanged.
- New subsystem requirements should appear in `SUBSYSTEM_ANATOMY_v2_1.md` or the owning subsystem spec, not only in prose elsewhere.
- Avoid future-doc dependencies unless the future doc is explicitly marked nonblocking.

## Done Means

- `docs/design/INDEX.md` read order still points to current canonical docs.
- Companion lists do not point active docs at archived docs except as source material.
- Stale-doc scans only leave intentional supersession notes.
