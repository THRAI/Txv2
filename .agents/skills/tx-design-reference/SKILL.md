---
name: tx-design-reference
description: Use when orienting on txKernel design docs, choosing canonical references, or gathering a reading set for implementation, audit, or planning work.
---

# tx-design-reference

Use this skill to gather the smallest relevant active-doc set before changing
architecture, writing implementation code, or auditing readiness.

## Read First

- `docs/design/INDEX.md`
- `docs/design/00_meta-framework/CONCEPTS_v4.md`
- `docs/design/00_meta-framework/INVARIANTS_v4.md`
- `docs/design/00_meta-framework/MODULE_MAP_v1.md`
- `docs/design/00_meta-framework/object_model_v2.md`
- `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`

## Topic Clusters

- HAL, boot, page substrate: `docs/design/01_substrate/HAL_v1.md`,
  `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md`, then use
  `tx-hal-axhal`.
- EBR, Zone, caps, witnesses: `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`,
  `docs/ebr-zone/02_EBR_design.en-US.md`,
  `docs/ebr-zone/03_Zone_Cap_object_storage_design.en-US.md`, then use
  `tx-ebr-zone`.
- Execution and scripts: `docs/design/02_execution/STEP_MODEL_v1.md`,
  `docs/design/02_execution/THREAD_RUNTIME_v1.md`,
  `docs/design/02_execution/EXEC_v1.md`.
- VM and page-backed storage: `docs/design/03_memory-vm/VM_v1_2.md`,
  `docs/design/03_memory-vm/PAGE_BACKED_v1.md`.
- Process and signals: `docs/design/04_process-signals/PROCESS_v1.md`,
  `docs/design/04_process-signals/SIGNAL_v1.md`,
  `docs/design/04_process-signals/SIGNAL_ATTACHMENTS_v1.md`.
- Filesystem and block devices: `docs/design/05_filesystem/`,
  `docs/design/06_devices/DEVICE.md`.
- TTY and character-device work: `docs/design/06_devices/TTY.md`,
  `docs/design/06_devices/DEVICE.md`.

## Rules

- Active docs in `docs/design/` override archived and source-trace docs.
- Use `docs/ebr-zone/` as mechanics reference material; treat
  `EBR_ZONE_INTERFACE_v1.md` and the meta-framework docs as the current
  architecture contract.
- Do not bulk-load directories when a topic cluster names the right files.
- Mention archived docs only as historical source material.

## Done Means

- The chosen reading set is explicit.
- Any implementation claim cites current active docs, not root-level stale paths.
- The task-specific skill is loaded next when one applies.
