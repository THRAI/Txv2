# M1 Dock mock pmap module split

## Context

After the high-half boot and kernel pmap lifecycle work,
`boards/tx-hal-riscv64-m1dock-mock/src/lib.rs` was close to the authored
source-size limit and mixed platform facade code with boot pmap state,
page-table mutation helpers, PT-node ownership, and private tests. Adding
process-root/ASID support there would have blurred the platform facade and made
future review harder.

## Decision

The M1 mock now has a board-private `src/pmap.rs` module. `lib.rs` keeps the
static platform selection facade: `_start`, `PlatformConfig`, console/power,
`PlatformInfo`, and `PmapIf` delegation. `pmap.rs` owns:

- high/direct/user topology constants;
- board-owned boot page-table statics and linker-section anchors;
- `BootInfo` and `BootstrapPmapInfo` publication;
- PT-node allocator installation and committed-node registry;
- kernel mapping reserve/rollback/commit/unmap/protect/shootdown hooks;
- private pmap tests.

The split is behavior-preserving. It does not introduce a shared M1 bag
abstraction and does not change the public HAL vocabulary.

## Verification

- `cargo fmt -p tx-hal-riscv64-m1dock-mock --check`
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo xtask build --target rv64-m1dock-mock`
- `RUSTFLAGS=-Dunused cargo check -p tx-kernel-riscv64-m1dock-mock --target riscv64gc-unknown-none-elf`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo clippy -p tx-hal-riscv64-m1dock-mock --all-targets -- -D warnings`
- `cargo xtask lint arch`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

Global `cargo fmt --check` was also attempted. It is currently blocked outside
this HAL/pmap slice by unrelated `tx-ext4-format` formatting diffs in
`crates/tx-ext4-format/src/ondisk.rs`, `crates/tx-ext4-format/src/pager.rs`,
and `crates/tx-ext4-format/tests/pager_mock.rs`.

## Next

Implement M1 process-root/ASID support inside `src/pmap.rs`, keeping `lib.rs` as
a narrow `PmapIf` facade.
