# Ext4 M1 High-DTB Boot Prerequisite

Date: 2026-08-09

## Change

The RV64 QEMU boot trampoline now installs the direct-map 1 GiB leaf that
contains the firmware DTB before Rust parses `BootInfo`. `FirmwareDtb` uses
that direct-map alias on RV64, while host tests retain their ordinary process
pointer. `extend_direct_map_from_bag` treats that preinstalled leaf as a
contiguous direct-map range member instead of rejecting it as already mapped.

This is the bounded boot prerequisite for the rustc witness: QEMU may place
the DTB above the initial bootstrap leaf when the witness requests its explicit
4096 MiB geometry. It changes neither the generic QEMU memory default nor any
filesystem, VFS, Mount, PageBacked, or SubmissionManager owner.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt -- --test-threads=1`: 95 passed,
  including `direct_map_extension_accounts_for_the_preinstalled_fdt_leaf`.
- `rustfmt --check --config skip_children=true` on each changed high-DTB board
  source passed. The recursive board check reaches pre-existing formatting
  drift in untouched `src/tests.rs`, so it is not evidence against this slice.

## Remaining Gate

This closes only M1/M2's high-DTB condition. The resolver installer must still
be versioned and idempotently scoped to the `WORKLOAD` image, and an RV64 QEMU
witness must still prove `rustc -vV` plus the offline frozen build across the
TEST, SCRATCH, and read-only WORKLOAD roles. No performance or Tier 1 candidate
claim follows from this boot repair.
