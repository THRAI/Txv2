# JBD2 codec reference slice

## Change

`tx-ext4-format::journal` now owns stateless big-endian codecs for the JBD2
common header, legacy 32-bit descriptor tags, full commit records, and revoke
records. The descriptor codec supports multiple tags, preserves legacy tag
checksum bytes and UUID placement, rejects unsupported tag flags, and requires
exactly one terminal `LAST` tag. Commit codec uses the 60-byte layout with the
eight checksum words and 64-bit commit seconds. Revoke codec validates its
on-disk byte count before decoding target blocks.

The shape follows `Starry-OS/rsext4` JBD2 record layout as reference material,
but no runtime code or cache model was imported. In particular, this slice does
not introduce a synchronous mutable block device, a second file-data cache, or
a journal cursor.

## Verification

- `cargo test -p tx-ext4-format --no-default-features -- --nocapture --test-threads=1`
  passed: 18 tests plus one skipped host-tool witness.
- `cargo check -p tx-ext4-format --no-default-features` passed.
- `git diff --check` passed for the codec slice.

## Next step and blocker

The next 6E sub-slice is an L5 `JournalTransactionPlan` that turns buffered
data predecessors, journal descriptor/data writes, and a durable commit fence
into `BackendBioGraph` nodes. `fsync` must remain on the compatibility path
until the graph is executed through L6 and crash/remount evidence proves the
ordered-mode contract. Checksum computation and 64-bit JBD2 tag support remain
explicitly deferred; the codec rejects those tag formats rather than parsing
them incorrectly.
