# JBD2 metadata page lease pool

## Change

`tx-ext4::journal::JournalPagePool` now owns a private persistent anonymous
`Cap<PageContainer>` for staged descriptor, metadata-copy, and commit records.
It materializes one 4 KiB page, copies an encoded record while holding the
short-lived map pin, drops that map pin, then exports a `PageLease`. The lease
keeps the physical frame live across L6 I/O and exports a `JournalBio` whose
`BioVec` and `IoDataSource::PageCache` reference the same PPN.

Pool slots are monotonic and never reused in this slice. Exhaustion therefore
fails before any possible reuse of a record still owned by an in-flight graph.
The PageContainer is private metadata storage, never a VFS RNode or ordinary
file-data cache.

## Verification

- `cargo test -p tx-ext4 --no-default-features -- --nocapture --test-threads=1`
  passed: 25 tests.
- `cargo check -p tx-ext4 --lib --no-default-features` passed.
- `git diff --check` passed for the pool slice.

## Next step and blocker

The next sub-slice must stage every page in a `Jbd2TransactionImage`, map the
staged leases to journal LBA locations, and build the corresponding
`JournalTransactionPlan`. The caller must retain the returned record leases
until terminal graph completion, then schedule checkpoint separately. This
does not yet wire ext4 fsync, driver durability, replay, or journal-space reuse.
