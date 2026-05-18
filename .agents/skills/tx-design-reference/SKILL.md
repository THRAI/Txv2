---
name: tx-design-reference
description: Use when orienting on txKernel design docs, choosing canonical references, or gathering a reading set for implementation, audit, or planning work.
---

# tx-design-reference

Use this skill to gather the smallest relevant active-doc set before changing
architecture, writing implementation code, or auditing readiness.

## Read First

- `docs/design/INDEX.md` — top-level read order across the v4 subsystem spine.
- `docs/Txv3/INDEX.md` — the v3 refresh; tells you which v4 docs are superseded and which still apply.
- `docs/Txv3/01_CONCEPTS_v5.md` — supersedes `CONCEPTS_v4.md`. Read this for new vocabulary (SubjectContext, YieldShape, ExecutionScope, primitive cells).
- `docs/Txv3/02_INVARIANTS_v5.md` — canonical invariant catalog. Adds SUBJ-*, YIELD-*, DELEGATE-*, SCOPE-* families; updates STEP-*.
- `docs/Txv3/03_STEP_MODEL_v2.md` — supersedes `STEP_MODEL_v1.md`. Four-variant `StepOutcome`, typed `StepOp`, anti-pattern catalog.
- `docs/design/00_meta-framework/CONCEPTS_v4.md` — still useful for the v4 sections v5 carries forward.
- `docs/design/00_meta-framework/INVARIANTS_v4.md` — section anchors that other v4 subsystem docs still cite.
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
- IPC (pipes, eventfd, signalfd, futex, sockets, SysV/POSIX IPC): **there is no
  dedicated `IPC_*.md` design doc yet** — IPC discussion is scattered. Read, in
  this order:
  - `docs/Txv3/00_PREFACE.md` — establishes that txKernel is not a microkernel
    and has no IPC boundary between subsystems (single address space; the
    "audit story" replaces process isolation).
  - `docs/design/06_devices/TTY.md` — the "IPC-producer-serialization pattern"
    section is the canonical write-up of how the ring/mutex/atomicity contract
    works for any subsystem with a kernel-internal queue (pipe, socket
    send/recv, POSIX message queue). Apply this pattern when designing or
    auditing any new buffer-based IPC primitive.
  - `docs/design/04_process-signals/SIGNAL_ATTACHMENTS_v1.md` — lists SysV IPC
    (`msg`, `sem`, `shm`) and POSIX IPC (`mq_open`, `sem_open`, `shm_open`) as
    **deferred** catalog entries pending dedicated subsystem specs. If you are
    here because you are about to *write* the missing SysV/POSIX IPC spec, this
    is your prior-art anchor and your obligation list (which signal carriers,
    which attachment shapes, which deferral notes to retire).
  - `docs/design/04_process-signals/SIGNAL_v1.md` and `PROCESS_v1.md` — signals
    are the most fundamental IPC primitive and provide the shape every other
    IPC primitive's wakeup/cancellation borrows from.
  - `docs/design/03_memory-vm/PAGE_BACKED_v1.md` — the shared-memory backing
    (shm/memfd/tmpfs) lives on `PageContainer`; any SysV `shm` or POSIX
    `shm_open` work must route through `RNodeBacking` rather than invent local
    page ownership.
  - Code (no design doc, but the closest to a spec for these primitives):
    `crates/tx-subsystems/src/{pipe,eventfd,signalfd,futex,signal,io_uring,userfaultfd,epoll}/`.
  - Decision history: `docs/progress/decisions/2026-05-12-d17-pipe-pilot-adapter.md`
    and other dated `*pipe*` / `*signal*` decisions under `docs/progress/decisions/`.

  When this changes — i.e. when a canonical `docs/design/04_process-signals/IPC_v1.md`
  or similar lands — this bullet should be replaced with that file's path, and the
  Manifest entry updated to match.

## Rules

- For meta-framework reading: `Txv3/` is the new spine. v4 docs are still canonical
  for everything the v3 docs don't touch (`docs/Txv3/INDEX.md` §3 spells out which is
  which). For *new prose* about concepts, invariants, or the step model, cite v5.
  For sections v5 carries forward unchanged, citing v4 is fine — and necessary,
  because v4 anchors are what other subsystem docs still resolve against.
- Active docs in `docs/design/` and `docs/Txv3/` override archived and source-trace docs.
- Use `docs/ebr-zone/` as mechanics reference material; treat
  `EBR_ZONE_INTERFACE_v1.md` and the meta-framework docs as the current
  architecture contract.
- Do not bulk-load directories when a topic cluster names the right files.
- Mention archived docs only as historical source material.

## Done Means

- The chosen reading set is explicit.
- Any implementation claim cites current active docs, not root-level stale paths.
- The task-specific skill is loaded next when one applies.
