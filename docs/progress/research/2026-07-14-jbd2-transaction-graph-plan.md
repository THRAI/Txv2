# JBD2 ordered transaction graph plan

## Change

`tx-ext4::journal::JournalTransactionPlan` now accepts L5-owned data,
descriptor, journal-metadata, commit, and checkpoint write leases. It validates
that every write targets one device and has a non-empty write bio, then emits:

1. a fsync-critical `BackendBioGraph` with `data -> barrier -> descriptor and
   metadata journal writes -> barrier -> FUA commit`; and
2. a separate checkpoint graph that the caller may submit only after the first
   graph has completed successfully.

The split means checkpoint latency does not extend fsync completion. Both
barriers are neutral L6 `BlockOp::Barrier` plans with `BARRIER | FLUSH` flags;
the commit write is cloned with `FUA`. The plan has no filesystem cache,
PageContainer, lock, driver, or completion ownership.

## Verification

- `cargo test -p tx-ext4 --no-default-features -- --nocapture --test-threads=1`
  passed: 24 tests.
- `cargo check -p tx-ext4 --lib --no-default-features` passed.
- `git diff --check` passed for the slice.

Existing warnings are unchanged: missing RISC-V host assembler causes the VDSO
stub warning, `step_connect` has an unused guard, and two private ext4 factory
constructors are currently unused.

## Next step and blocker

The next sub-slice must create real transaction state in `tx-ext4`, encode the
descriptor/metadata/commit buffers with `tx-ext4-format::journal`, retain their
frame leases through graph completion, and only then route fsync through the
graph. The current L6 driver path must also prove that `Barrier` and `FUA`
reach a device durability primitive before fsync gains any persistence claim.
Mount replay, checksum validation, journal wrap accounting, and crash/remount
proof remain open.
