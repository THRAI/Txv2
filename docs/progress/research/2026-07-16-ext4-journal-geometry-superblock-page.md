# Ext4 Journal Geometry Retains Superblock Page

Date: 2026-07-16

## Change

`Ext4Pager::journal_geometry` now returns the original JBD2 superblock page
alongside parsed geometry and logical-to-physical journal block mapping. The
page is optional because pure replay does not mutate journal state; a future
ring-backed writer will require it to preserve extension fields while staging
activation and clean-state `s_start/s_sequence` writes.

## Verification

- `cargo test -p tx-ext4-format --test pager_mock pager_derives_journal_ring_from_journal_inode_mapping -- --nocapture`
- `cargo test -p tx-ext4-format --test jbd2_recovery -- --nocapture`
- `cargo check -p tx-ext4-format`

## Next

Extend `JournalRingReservation` and `PreparedJournalTransaction` so a journal
activation page is durable before descriptor/commit and a clean-state page is
durable only after metadata checkpoint. Keep both pool leases through their
respective graph terminal completion.
