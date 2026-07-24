# JBD2 transaction image builder

## Change

`tx-ext4-format::journal::Jbd2TransactionImage::encode_legacy` now transforms
a closed list of metadata home-block updates into the three JBD2 record shapes
that L5 will later place in journal frames: one 4 KiB descriptor page, ordered
4 KiB metadata journal copies, and one 4 KiB commit page. The first descriptor
tag carries the supplied journal UUID and later tags use `SAME_UUID`; the final
tag carries `LAST`.

If a copied metadata block begins with the big-endian JBD2 magic, the journal
copy is zeroed at that word and its tag carries `ESCAPE`. Replay must restore
the magic after validating the committed descriptor. The builder uses the pure
record codecs already committed in `8feee787`; it has no device, cache, lock,
or runtime dependency.

## Verification

- `cargo test -p tx-ext4-format --no-default-features -- --nocapture --test-threads=1`
  passed: 19 tests plus one skipped host-tool witness.
- `cargo check -p tx-ext4-format --no-default-features` passed.
- `git diff --check` passed for the builder slice.

## Next step and blocker

The next 6E slice must allocate/lease actual journal record frames, pair their
`BioVec`s with `JournalTransactionPlan`, and retain them until terminal graph
completion. The pure image bytes cannot yet be submitted to L6. CRC32c commit
and descriptor checksums, 64-bit tags, revoke application, and mount-time
replay remain open.
