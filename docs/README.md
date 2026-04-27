# Documentation Guide

The active docs are collected here so the repository root stays implementation
focused. Architecture contracts live in `design/`, imported EBR/Zone mechanics
live in `ebr-zone/`, and durable project memory lives in `progress/`.

## Active Architecture

Read in this order:

1. [`design/INDEX.md`](design/INDEX.md)
2. [`design/00_meta-framework/CONCEPTS_v4.md`](design/00_meta-framework/CONCEPTS_v4.md)
3. [`design/00_meta-framework/INVARIANTS_v4.md`](design/00_meta-framework/INVARIANTS_v4.md)
4. [`design/00_meta-framework/object_model_v2.md`](design/00_meta-framework/object_model_v2.md)
5. [`design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`](design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md)

Layer directories:

- [`design/00_meta-framework/`](design/00_meta-framework/)
- [`design/01_substrate/`](design/01_substrate/)
- [`design/02_execution/`](design/02_execution/)
- [`design/03_memory-vm/`](design/03_memory-vm/)
- [`design/04_process-signals/`](design/04_process-signals/)
- [`design/05_filesystem/`](design/05_filesystem/)
- [`design/06_devices/`](design/06_devices/)

## EBR And Zone Reference

The imported implementation sketches are reference material, not top-level
architecture contracts:

- [`ebr-zone/02_EBR_design.en-US.md`](ebr-zone/02_EBR_design.en-US.md)
- [`ebr-zone/03_Zone_Cap_object_storage_design.en-US.md`](ebr-zone/03_Zone_Cap_object_storage_design.en-US.md)

## Workspace And Tooling

- [`DEVELOPMENT.md`](DEVELOPMENT.md) describes the Rust workspace, `xtask`,
  QEMU, OSComp, images, M1 Dock mock target, and clean K210 submit tree.
- [`../xtask/README.md`](../xtask/README.md) describes the `cargo xtask`
  command implementation layout.
- [`progress/README.md`](progress/README.md) describes how decisions, plans,
  handoffs, research notes, and current status are recorded.
- Archived historical docs remain under
  [`design/00_meta-framework/archived/`](design/00_meta-framework/archived/) and are not
  implementation contracts.

## Cleanup Rules

Active docs override archived and source-trace notes. Do not resurrect v11,
OSTD HAL manager, runtime `HalManager`, or upper-layer raw `Zone<T, Policy>`.
Active design docs use grep-stable `txdoc:` tags for CI and review references;
see [`design/00_meta-framework/CI_REPORTING_v1.md`](design/00_meta-framework/CI_REPORTING_v1.md).
After doc edits, run:

```sh
cargo xtask lint docs
cargo xtask lint arch
```
