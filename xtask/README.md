# txKernel Xtask

`xtask` is the public developer API for the workspace. Shell scripts may wrap
it, but orchestration should live here.

## Module Map

- `main.rs` is only the binary entrypoint.
- `lib.rs` owns command dispatch and usage text.
- `doctor.rs` checks local toolchain, target, QEMU, image, and submodule setup.
- `check_build.rs` owns `check` and `build`.
- `ci.rs` owns CI reporting and design-doc `txdoc:` references.
- `lint.rs` owns architecture and Markdown documentation lints.
- `progress/` owns schema-tagged JSON progress memory commands and validation.
- `qemu.rs` owns emulator command construction and launching.
- `image.rs` owns BusyBox cpio, ext4, and M1 Dock SD image builders.
- `oscomp.rs` owns OSComp autotest preparation, submission, and QEMU helpers.
- `submit.rs` owns clean contest submit-tree generation.
- `target.rs` owns target aliases, target triples, and rustup target checks.
- `unit.rs` owns the `unit` command: compact build + host unit-test runner.
- `util.rs` owns filesystem, command, path, and formatting helpers shared by
  command modules.

## Rules

- Keep new command families in their own module.
- Keep `lib.rs` as dispatch glue, not a dumping ground.
- Put shared helpers in `util.rs` only when at least two command modules need
  them.
- Prefer adding focused tests beside the module that owns the rule.
- Run `cargo test -p xtask` before declaring command behavior ready.

## QEMU Sentinel Contract

`cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel`
captures serial output in `target/qemu-rv64-qemu-smoke.serial.log` and passes
only after observing `txkernel:qemu-riscv64-virt:boot:ok`.

`cargo -q xtask unit` is the fast local check: builds tx-shims, tx-kernel,
tx-ext4, and tx-scripts, then runs each lib test suite. One line per step on
pass; failures show only the failing test, panic message, and failure list.

`cargo xtask ci` remains the fast compile/lint lane. `cargo xtask ci-slow`
builds RV64 QEMU and runs the smoke sentinel lane.
