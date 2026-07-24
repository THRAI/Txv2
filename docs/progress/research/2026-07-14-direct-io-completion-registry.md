# Direct-I/O completion registry (2026-07-14)

## Change

`PageContainer` now owns a direct-I/O in-flight registry keyed by
`IoDataLeaseId`. Admission combines the existing direct range reservation with
an owned `DirectIoBuffer`; it returns an immutable neutral descriptor carrying
either `IoDataTarget::Direct` for reads or `IoDataSource::Direct` for writes.
The pinned buffer remains private to PageBacked until terminal completion.

Completion removes the in-flight row under the short PageContainer state lock,
then releases that lock before applying the existing direct-read or
direct-write coherency path. A successful direct write conservatively
invalidates clean cache entries, while a direct read preserves them. Dropping
the withdrawn buffer releases its DMA pins only after that terminal path has
run. Duplicate or late completion returns `UnknownLease`.

## Evidence

- `cargo test -p tx-subsystems --lib page_backed --no-default-features -- --nocapture --test-threads=1`: 134 passed.
- `cargo check -p tx-subsystems --lib --no-default-features`: passed.
- `cargo xtask progress validate`: passed.

The added end-to-end unit case verifies read-target and write-source neutral
descriptors, in-flight retention, terminal range release, direct-write clean
cache invalidation, and duplicate completion rejection.

## Boundary

This is L4 state ownership only. It does not submit the descriptor through an
ext4 planner or L6 driver, expose an `O_DIRECT` syscall, zero-fill holes, or
provide `msync`/`syncfs` coherency. The compatibility `FsPageBacking` path
remains live.
