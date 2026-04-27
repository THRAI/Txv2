# txKernel Status

**Updated:** 2026-04-28

## Current Shape

- Rust workspace skeleton exists with `cargo xtask` as the developer command
  surface.
- QEMU RV64, LA64, and RV64 M1 Dock mock target wiring exists as compile-first
  stubs.
- OSComp autotest is present as the `external/oscomp-autotest` submodule.
- BusyBox cpio, BusyBox ext4, and M1 Dock SD-image builder contracts exist.
- Clean K210 submit-tree generation is available through `cargo xtask submit k210`.
- Active design docs are collected under `docs/design/`.
- Imported EBR/Zone mechanics references are collected under `docs/ebr-zone/`.
- Durable progress memory lives under `docs/progress/`.
- Plans, handoffs, and worktree records use schema-tagged JSON for
  agent-friendly queries.
- `cargo xtask progress` can validate, list, create, claim, and close
  operational JSON records.
- `xtask` is split by command family under `xtask/src/`, with a local module
  map in `xtask/README.md`.
- HumanLayer `.claude` workflow references are available as a sparse submodule
  at `external/humanlayer-reference`.
- `cargo xtask ci` provides concise CI reporting with `txdoc:` references into
  the active design docs.
- Active design docs now carry fine-grained `txdoc:` anchors; docs lint rejects
  top-only anchoring.
- RV64 QEMU now has an ArceOS-style platform-owned boot path: linker script,
  `_start`, BSS clearing, SBI console output, typed `BootHandoff`, and the
  smoke sentinel `txkernel:qemu-riscv64-virt:boot:ok`.
- RV64 QEMU publishes BootInfo v1 from the OpenSBI-provided DTB: usable memory
  regions, kernel image linker bounds, chosen bootargs, and initrd bounds.
- RV64 QEMU now enables an Sv39 bootstrap pmap before `rust_entry`: a 1 GiB
  identity leaf for QEMU RAM plus a fixed early PT-node pool exposed through
  `PmapIf`.
- `cargo xtask ci-slow` runs the RV64 QEMU smoke sentinel lane separately from
  fast compile/lint CI.
- The active HAL, page-substrate, module-map, and invariant docs now state the
  portable boot contract later platforms must follow.
- Agent workflow now requires a finish catch-up in `docs/progress/` before any
  completed task is declared done; see
  `docs/progress/decisions/2026-04-28-finish-catchup-progress-memory.md`.
- This foundational workspace snapshot is ready to publish to the Txv2 remote:
  it captures the Rust skeleton, xtask tooling, docs/progress memory, OSComp and
  HumanLayer references, RV64 QEMU smoke boot, BootInfo v1, and bootstrap pmap.

## Open Blockers

- Real K210 boot, linker, and hardware path are not implemented yet.
- OSComp FAT32 image/test runner integration is not yet a passing boot test.
- LA64 target availability depends on local rustup support.
- LA64 and M1 Dock mock boot protocols are compile-first only.
- RV64 QEMU still needs high-half/direct-map completion, full pmap
  reserve/commit/unmap, shootdown integration, and a minimal trap vector before
  the page substrate is substrate-ready.
- ext4 image creation requires host `mkfs.ext4`.
- BusyBox images require `TX_BUSYBOX`; dynamic musl layouts also require
  `TX_MUSL_LIBC`.

## Latest Decisions

- `docs/progress/decisions/2026-04-28-finish-catchup-progress-memory.md`
- `docs/progress/decisions/2026-04-28-arceos-aligned-portable-boot.md`
- `docs/progress/decisions/2026-04-27-fine-grained-txdoc-anchors.md`
- `docs/progress/decisions/2026-04-27-xtask-module-split.md`
- `docs/progress/decisions/2026-04-27-xtask-progress-command-surface.md`
- `docs/progress/decisions/2026-04-27-ci-reporting-and-txdoc-tags.md`
- `docs/progress/decisions/2026-04-27-json-agent-operational-records.md`
- `docs/progress/decisions/2026-04-27-humanlayer-reference-and-agentic-workflow.md`
- `docs/progress/decisions/2026-04-27-doc-layout-and-progress-memory.md`

## Latest Research

- `docs/progress/research/2026-04-27-humanlayer-progress-memory.md`
