# RCU-on-VM to SMP migration closeout

## Decision

The SMP worktree adopts the verified ResidentRoot/EBR/PageBacked slices from
`/Users/3y/.config/superpowers/worktrees/Tx/rcu-on-vm` without importing its
scheduler, HAL, ext4, network, PELT, or broad VM range-transaction changes.
`ResidentRoot` is immutable guarded observation state; PageSlot, I/O custody,
RangeLock, MapPin, PTE materialization, and TLB shootdown remain their existing
owners.

Maintenance IPI handling stays IRQ-safe: the trap acknowledges the IPI and sets
a per-hart pending bit. The reactor consumes that bit in normal context and
services at most 64 local EBR retire entries per turn before zone maintenance.
This keeps maintenance work bounded while preserving local retirement ownership.

## Regression Fixes

Resident-root publication backpressure is temporary `EAGAIN`; the file-page
owner returns `Continue` so a drive turn can retry under a fresh guard. The four
ext4 regressions were caused by tests retaining one epoch Guard across mkdir,
create, write, and metadata operations, plus a direct journal admission witness
that retained staged resources. The tests now release those resources and drain
to quiescence between semantic phases.

## Verification

- `cargo -q xtask unit`: tx-shims 663, tx-kernel 119, tx-ext4 73 (2 ignored),
  tx-scripts 168.
- `cargo xtask full-build --target rv64-qemu`: passed.
- `cargo xtask qemu --target rv64-qemu --profile smoke --smp 1
  --expect-sentinel --timeout-ms 90000`: boot sentinel passed.
- Same command with `--smp 2`: boot, owner-wake, and all six RCU markers passed.

## Deferred Boundary

The target tree has no `vm/range_txn.rs`, so the source worktree's
`init_vm_range_txn_hart` and `drain_local_vm_cleanup` are not part of this
closeout. That per-hart VM cleanup integration requires a separate recipe/
range-transaction plan. This closeout makes no full-VM SMP reclamation claim.
