---
name: tx-implementation-readiness
description: Use when deciding whether a txKernel doc area is ready to implement from, or when producing an implementation-readiness audit.
---

# tx-implementation-readiness

Use this skill to answer "can we code from these docs?"

## Read First

- `docs/design/INDEX.md` and `docs/Txv3/INDEX.md`
- `docs/Txv3/02_INVARIANTS_v5.md` — canonical invariant catalog (v5 supersedes v4 for new prose; v4 still applies for sections v5 doesn't touch).
- `docs/Txv3/03_STEP_MODEL_v2.md` — four-variant `StepOutcome` and the typed `StepOp` trait; the anti-pattern catalog (A-1..A-15) is itself a readiness rubric.
- `docs/Txv3/07_BLAST_RADIUS.md` — quantifies migration cost; useful when readiness depends on how much code a doc change implies.
- `docs/design/00_meta-framework/INVARIANTS_v4.md` — section anchors still cited by v4 subsystem docs.
- `docs/design/00_meta-framework/object_model_v2.md`
- `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`
- Domain docs for the subsystem being assessed

## Readiness Rubric

An area is implementation-ready when:

- The owning architectural home is clear.
- Public types are named or derivable.
- Entity/reference roles are classified.
- Binding obligations and stored evidence are stated.
- Step phases name observe, upgrade, reserve, commit, publish behavior.
- Reclamation/lifetime rules are tied to `ZONE-*`, `EBR-*`, `BIF-*`, `OBL-*`, and `WIT-*`.
- Cross-subsystem dependencies point to current active docs.
- Remaining deferrals are explicit and nonblocking.

## Output Shape

```text
Ready: yes / no / mostly
Blocking gaps:
- ...
Nonblocking follow-ups:
- ...
Implementation entry order:
1. ...
```

## Done Means

- The answer distinguishes architecture gaps from code API spelling gaps.
- Any "not ready" item names the doc and section that must change.
