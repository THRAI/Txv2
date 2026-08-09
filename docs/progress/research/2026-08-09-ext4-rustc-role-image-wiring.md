# Ext4 Rustc Role-Image QEMU Wiring

Date: 2026-08-09

## Change

`cargo xtask qemu` and `cargo xtask shell-test` now accept three named RV64
role-image arguments:

- `--ext4-test-image` maps TEST at `vda` and publishes `tx.ext4.test=vda`.
- `--ext4-scratch-image` maps SCRATCH at `vdb` and publishes
  `tx.ext4.scratch=vdb`.
- `--ext4-workload-image` maps WORKLOAD at `vdc`, publishes
  `tx.ext4.workload=vdc`, and adds QEMU `read-only=on`.

The existing repeatable `--extra-rv64-ext4` path remains for Tier 1 callers.
It cannot be mixed with named roles, and two named roles cannot name the same
image. These checks run before QEMU starts. When RV64 networking is enabled,
the net device is placed after the role buses (bus 3 for all three roles), so
it cannot alias TEST/SCRATCH/WORKLOAD. No global memory default changed.

## Verification

- `cargo test -p xtask -- --test-threads=1`: 450 passed.
- Focused role rerun: qemu 24 and shell-test 12 passed, including the RV64
  net-after-role bus regression.
- `cargo xtask ext4 tier1 --dry-run`: passed; the legacy role-image invocation
  still produces the full Tier 1 action plan.
- `rustfmt --check xtask/src/qemu.rs xtask/src/shell_test.rs`
- `git diff --check`

## Remaining Gate

This wires role transport only. It does not materialize a measured WORKLOAD
image, install the resolver shim into only that image, change the generic
Alpine memory profile, or produce an RV64 `rustc -vV` / offline frozen build
witness. M1/M2 and M3 therefore remain pending.
