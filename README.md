# txKernel

txKernel is a Rust kernel architecture and implementation workspace. The active
architecture docs define a factored kernel model: semantic subsystems own
entities and transitions; substrate provides zone, index, epoch, mutation, bus,
page, and reservation primitives; the reactor schedules tasks and waits; HAL is
an axHal-style static platform family.

## Start Here

- [`docs/design/INDEX.md`](docs/design/INDEX.md) is the active architecture
  index.
- [`docs/README.md`](docs/README.md) explains how the docs are organized.
- [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) covers tooling, QEMU, OSComp,
  images, and clean submit-tree generation.
- [`docs/progress/README.md`](docs/progress/README.md) records durable plans,
  decisions, handoffs, and status.
- [`AGENTS.md`](AGENTS.md) records agent guidance and canonical read order.
- [`external/humanlayer-reference/`](external/humanlayer-reference/) contains
  the reference-only HumanLayer `.claude` workflow prompts.

## Workspace Map

- `crates/` contains architecture-level Rust crates shared by all boards.
- `boards/` contains static board HAL crates and board binary crates.
- `xtask/` is the single developer command surface; see
  [`xtask/README.md`](xtask/README.md) for the command module layout.
- `tools/` contains thin wrappers that delegate to `cargo xtask`.
- `docs/` contains active design docs, imported EBR/Zone references, and
  durable progress memory.
- `external/` contains reference submodules used by tooling and agent workflow
  research.

## Common Commands

```sh
cargo xtask doctor
cargo xtask ci
cargo xtask ci-slow
cargo xtask check
cargo xtask build --target rv64-qemu
cargo xtask build --target rv64-m1dock-mock
cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel
cargo xtask qemu --target rv64-m1dock-mock --profile smoke --dry-run
cargo xtask progress list all --json
cargo xtask submit k210
```

Generated build outputs, image roots, and submit trees live under `target/`.
They are disposable and should be regenerated through `cargo xtask`.
