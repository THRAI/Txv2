# Ext4 xattr follow-up

## Context

The 2026-05-25 VFS xattr slice added backend hooks plus tmpfs storage, then the
follow-up stack brought ext4 to a narrow but real `user.*` implementation.
Ext4 no longer inherits the `FsOps` default for supported xattr operations; the
remaining work is deeper ext4 storage policy, not syscall/VFS plumbing.

## Current readiness

- `external/rsext4` at the pinned reference has inode fields such as
  `file_acl`, `extra_isize`, and feature constants, but no reusable
  get/list/set/remove xattr implementation.
- Tx ext4 now has parsers and encoders for inline inode-body and external
  `i_file_acl` xattr-block `user.*` entries and wires them through
  `FsOps::get_xattr` / `FsOps::list_xattr` / `set_xattr` / `remove_xattr`.
  External blocks validate the Linux header shape (`h_magic`, `h_blocks == 1`)
  and metadata checksum when the filesystem advertises `metadata_csum`; writes
  refresh the same checksum. EA-inode values and non-`user.*` namespaces remain
  deliberately unsupported.
- Tx ext4 now has a narrow metadata transaction path for supported xattr
  writes and chmod/chown. Descriptor/payload/commit records are barrier-ordered
  and synchronously checkpointed back to home metadata blocks. Xattr writes
  remain deliberately scoped to whole-set rewrite of `user.*` entries, inline
  when it fits and one external block otherwise.
- Linux ext4 xattrs span inline inode-body entries, external xattr blocks,
  checksums and hashes, shared-block refcounts, optional EA inode storage, and
  journal credit accounting.

## Follow-up order

1. DONE 2026-05-25: Add read-only ext4-format parsing for inline inode-body
   `user.*` xattrs and expose `get_xattr`/`list_xattr` for those entries.
2. DONE 2026-05-25: Extend read-only parsing to external xattr blocks,
   including Linux-shaped header validation and `metadata_csum` checksum
   verification. Hash/refcount mutation policy remains write-side work.
3. DONE 2026-05-25: Add narrow ext4 `user.*` xattr set/remove support using
   Linux entry/block layout rules and rsext4-inspired inode/block rewrite
   plumbing. The v1 writer rewrites the complete user-xattr set, uses inline
   inode-body storage when it fits, and falls back to/updates one external
   xattr block.
4. DONE 2026-05-25: Land Tx-native metadata transactions for supported
   xattr set/remove and chmod/chown. The implementation is metadata-only,
   synchronous-checkpoint v1, not full Linux JBD2.
5. Extend metadata transactions to truncate, page flush/fsync metadata,
   multi-block xattr growth, shared-block COW/refcount mutation, and block
   free on last xattr removal.
6. After that, revisit ACL, file-capability, and security namespaces; those
   still require their owning policy subsystems.

## Next xattr followups

- Shared external xattr block handling: detect `h_refcount > 1`, implement COW
  to a new block, and journal old-block refcount decrement plus new block,
  inode pointer, bitmap, and group descriptor updates together.
- Removal/free path: when the last external `user.*` entry is removed, clear
  `i_file_acl`, free the xattr block, decrement inode `blocks_512`, and journal
  bitmap/group descriptor/inode updates atomically.
- Multi-block and EA-inode values: keep returning `ENOSYS` until ext4 supports
  EA-inode allocation, ownership, and lifecycle accounting.
- Namespace followups: `system.posix_acl_*`, `security.*`, `trusted.*`, and file
  capabilities stay blocked on ACL/security/exec policy owners.
- Metadata transactions: reuse the new transaction primitive for truncate,
  page-flush metadata, fsync/checkpoint policy, and directory mutation cleanup.

## Verification from this slice

- `cargo test -p tx-fs tmpfs_xattr -- --nocapture`
- `cargo test -p tx-shims --lib xattr -- --nocapture`
- `cargo check -p tx-shims -p tx-subsystems -p tx-fs -p tx-ext4 -p tx-ext4-format`
- `cargo test -p tx-ext4-format xattr -- --nocapture`
- `cargo test -p tx-ext4 xattr -- --nocapture`
- `cargo test -p tx-ext4 --lib tests_v3 -- --nocapture`
- `cargo test -p tx-ext4 --features host-async --test async_adapter -- --nocapture`
- `cargo check -p tx-ext4 -p tx-ext4-format -p tx-fs`
