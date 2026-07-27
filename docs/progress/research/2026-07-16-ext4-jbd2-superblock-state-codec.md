# Ext4 JBD2 Superblock State Codec

Date: 2026-07-16

## Change

Added `Jbd2Superblock::write_state`. It mutates an existing 4 KiB journal
superblock page in place, validates that the immutable journal geometry and
UUID still match the discovered superblock, and changes only `s_sequence` and
`s_start`.

For JBD2 checksum v2/v3, it requires CRC32C checksum type 4, zeroes
`s_checksum` at offset `0xFC`, computes the Linux-compatible raw CRC32C from
seed `~0` over the complete 1024-byte superblock, and restores the big-endian
checksum. Other bytes, including future or mount-specific fields, are
preserved.

## Verification

- `cargo test -p tx-ext4-format --test jbd2_codec superblock_state_update_preserves_unknown_bytes_and_recomputes_crc32c -- --nocapture`

## Next

Stage the journal superblock activation page before descriptor/metadata/commit
and the clean-state page only after durable checkpoint. The runtime must retain
those page leases along with the existing record leases, then initialize its
sequence from replay before guest RW mount wiring is enabled.
