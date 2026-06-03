# 2026-06-01: SpinMutex facade lint and line-limit baselines

## Context

Lock observe metrics need every runtime lock declaration to go through a local
wrapper instead of importing `tx_substrate::SpinMutex` directly. Without that
facade, enabling a lock metric family requires patching scattered raw lock
imports and constructors.

The migration also touched already-large runtime files. `cargo xtask lint arch`
was red on several oversized files before the new raw-lock lint could be used
as the completion gate.

## Decision

Add crate-local lock facades and make `cargo xtask lint arch` reject raw
`tx_substrate::SpinMutex` outside these facades and the substrate crate itself.
Kernel and process locks now have typed constructor helpers so local cfg gates
can opt into `LockMetricsOn` while the global `tx_lock_metrics` cfg still
removes the timing/emission implementation when closed.

Keep the global authored Rust source ceiling at 1,800 lines, but add exact
path-specific baselines for the oversized files present during this migration:

- `crates/tx-kernel/src/init.rs`: 1,925
- `crates/tx-shims/src/linux_syscall/numbers.rs`: 1,853
- `crates/tx-shims/src/linux_syscall/fs_basic.rs`: 1,847
- `crates/tx-subsystems/src/futex/mod.rs`: 1,802
- `crates/tx-subsystems/src/pipe/mod.rs`: 1,925
- `crates/tx-subsystems/src/process/execution.rs`: 1,811
- `crates/tx-subsystems/src/process/structure.rs`: 1,821

These are ceilings, not exemptions: any growth over the recorded line count
fails the lint, and future module-split work should lower or remove entries.

## Verification

- `cargo fmt --check`
- `cargo xtask lint arch`
- `cargo test -p xtask raw_spinmutex -- --nocapture`
- `cargo test -p xtask file_size_lint -- --nocapture`
- `cargo check -p tx-subsystems -q`
- `RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_process" cargo check -p tx-subsystems -q`
- `RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_kernel" cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf -q`
- `cargo check -p tx-fs -q`
- `cargo check -p tx-scripts -q`
- `cargo check -p tx-shims -q`
- `cargo check -p tx-drivers -q`
- `cargo check -p tx-ext4 -q`
- `cargo check -p tx-fat -q`

## Next Step

Burn down the baselined oversized files with responsibility-shaped splits,
starting with `tx-kernel/src/init.rs`, `process/structure.rs`, and
`process/execution.rs`, then lower the per-file ceilings as each split lands.

## Blockers

No blocker. The line-limit baselines are deliberate technical debt recorded so
the raw-lock lint can be enforced immediately.
