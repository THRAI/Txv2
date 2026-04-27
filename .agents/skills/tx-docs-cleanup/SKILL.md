---
name: tx-docs-cleanup
description: Use when doing a broad txKernel documentation cleanup, stale-reference pass, active-link audit, or cross-doc alignment walk.
---

# tx-docs-cleanup

Use this skill for cleanup walks across active docs. Keep the parent context small: inspect, patch, and report only the decisions that matter.

## Read First

- `AGENTS.md`
- `docs/design/INDEX.md`
- `docs/design/00_meta-framework/CONCEPTS_v4.md`
- `docs/design/00_meta-framework/INVARIANTS_v4.md`
- `docs/design/00_meta-framework/CI_REPORTING_v1.md`
- `docs/design/00_meta-framework/MODULE_MAP_v1.md`

## Rules

- Active docs override archived and source-trace docs.
- Do not rewrite archived docs unless asked.
- Do not let active docs depend on `OBJECT_MODEL_v3`, v11, old OSTD HAL manager, or stale unversioned names.
- Link labels should match current filenames unless the text is explicitly historical.
- Active design docs must keep unique `txdoc:` tags, with at least one
  section-level tag beyond the file-level tag.
- Broad cleanup should end with active link validation and stale-term scans.

## Common Scans

```sh
rg -n "CONCEPTS_v3|INVARIANTS_v3_3|OBJECT_MODEL_v3|tx-kernel-architecture-v11|HalSignalHooks|HalManager|__ostd_main|VFS_CHECKS_V2_1|PAGE_BACKED_v1__1_" \
  docs/design docs/ebr-zone \
  --glob '!docs/design/00_meta-framework/archived/**' --glob '!docs/design/00_meta-framework/FRAMEWORK_HARVEST_v1.md' --glob '!docs/design/00_meta-framework/INVARIANT_LEDGER_v1.md'
```

```sh
rg -n "OSTD|ostd|WeakCap|weak_count|dyn Hal|HAL signal-hook" \
  docs/design docs/ebr-zone \
  --glob '!docs/design/00_meta-framework/archived/**' --glob '!docs/design/00_meta-framework/FRAMEWORK_HARVEST_v1.md'
```

## Done Means

- Active Markdown links resolve.
- Remaining stale terms are intentional supersession, migration, or negative-rule notes.
- Any touched subsystem spec still has a zone-derived type policy section if it owns or consumes reclaimable entities.
