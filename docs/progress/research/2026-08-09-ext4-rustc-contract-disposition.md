# Ext4 Rustc Contract Disposition

Date: 2026-08-09

## Integrated Slice

`tools/ext4/workloads/rustc-kernel-build.json` is now present as the bounded
native-RV64 rustc workload contract fixture. It names the intended offline,
frozen kernel build, TEST/SCRATCH/WORKLOAD roles, and 1 CPU / 4096 MiB
geometry. Its `fixture-non-evidence` status and zero placeholders are
intentional: it cannot authorize workload-image materialization, a QEMU claim,
or a performance receipt.

## Deferred Slice

The historical rustc/performance worktree also contains an older
`xtask/src/ext4.rs` `perf` command path. The current reconciliation worktree
uses the later `xtask/src/ext4/mod.rs` Tier 1 runner and exposes only `tier1`.
Importing the old command would recreate an obsolete module interface and
would not wire a valid current runner. The guest scripts, resolver shim,
materializer, and paired-receipt changes further rely on absent current
SubmissionManager, QEMU, and generated-image bindings.

M1/M2 therefore remains pending on a current-interface implementation with
measured non-placeholder inputs and a bounded RV64 guest witness. M3 remains a
handoff to the existing SubmissionManager plan. No filesystem ownership or
VFS/Mount/PageBacked interface changed in this slice.

## Verification

- `jq empty tools/ext4/workloads/rustc-kernel-build.json`
- `git diff --check`
- `cargo xtask lint docs`
- `cargo xtask progress validate` remains blocked only by the unrelated missing
  `docs/progress/research/2026-08-04-smp-scheduler-readiness-audit.md`
  reference in the SMP plan.
