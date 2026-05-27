---
name: tx-ebr-zone
description: Use when editing EBR, zone, object model, cap, weak, witness, identity-slot, projection-reference, or reclamation-policy docs.
---

# tx-ebr-zone

Use this skill when the work touches zone allocation, EBR, reference layering, reclamation, or upper-language type translation.

## Read First

- `docs/design/00_meta-framework/object_model_v2.md`
- `docs/Txv3/02_INVARIANTS_v5.md` — canonical invariants. ZONE-*/EBR-*/OBL-*/WIT-* families are carried forward from v4; SUBJ-*/SCOPE-* are new and touch identity-slot reasoning.
- `docs/design/00_meta-framework/INVARIANTS_v4.md` §4A — still the section anchor other v4 subsystem docs cite for zone/EBR rows.
- `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`
- `docs/ebr-zone/02_EBR_design.en-US.md`
- `docs/ebr-zone/03_Zone_Cap_object_storage_design.en-US.md`

## Preserve

- Policy-based zones are the universal lifetime substrate for reclaimable upper entities.
- Upper subsystem APIs expose role-shaped types, not raw `Zone<T, Policy>`.
- `Weak<T>` is generation-checked and non-retaining.
- `IdentRef<'g, T>` is guard-scoped EBR observation and does not cross step boundaries.
- `Cap<T>` is identity retention.
- `PayloadCap<T>` or `T::OperationalEvidence` pins payload or a typed contribution.
- Split entities reach payload evidence through identity.

## Translation Rule

```text
entity shape + binding obligation + operation need
  -> zone-backed type family
  -> stored evidence type
  -> witness / upgrade API
```

## Done Means

- No semantic operation code/prose selects `RcPolicy`, `EbrPolicy`, or `Zone<T, Policy>` at call sites.
- Subsystem docs touched by the change have a zone-derived type policy table.
- `ZONE-*`, `EBR-*`, `OBL-*`, and `WIT-*` invariants still agree.
