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
  refresh the same checksum. Read-only EA-inode values are resolved for
  `user.*` entries after validating `EXT4_EA_INODE_FL`, value size, inode
  hash, and entry hash; write-side EA-inode creation/refcount/free remains
  deliberately unsupported.
- Tx ext4 now has a narrow metadata transaction path for supported xattr
  writes and chmod/chown. Descriptor/payload/commit records are barrier-ordered
  and synchronously checkpointed back to home metadata blocks. Xattr writes now
  cover whole-set rewrite of `user.*` entries, inline when it fits, one
  external block otherwise, shared-block COW, and freeing a one-block external
  xattr block when the remaining set moves inline. Oversized values remain
  explicitly unsupported, with a regression test proving failed allocation
  preparation does not dirty in-memory accounting before a later commit.
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
5. DONE 2026-05-25: Add shared external block COW/refcount decrement plus
   free-on-last-removal for one-block `user.*` xattr storage.
6. DONE 2026-05-25: Fence oversized/EA-inode-style xattr values with explicit
   unsupported errors and no accounting drift on failed allocation prep.
7. DONE 2026-05-25: Add read-only EA-inode-backed `user.*` values, including
   multi-block extent reads and Linux-shaped hash/size/flag validation. Existing
   EA-inode entries make set/remove return `ENOSYS` to avoid refcount leaks.
8. Extend metadata transactions to truncate, page flush/fsync metadata,
   multi-block xattr growth, and EA-inode backed values once the EA-inode
   lifecycle policy exists.
9. After that, revisit ACL, file-capability, and security namespaces; those
   still require their owning policy subsystems.

## Next xattr followups

- EA-inode writes: keep returning `ENOSYS` until ext4 supports EA-inode
  allocation, ownership, data-block writes, orphan/free, and lifecycle
  accounting.
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
- `cargo test -p tx-ext4-format --test pager_mock xattr_ -- --nocapture`
- `cargo test -p tx-ext4-format --test pager_mock -- --nocapture`
- `cargo xtask progress validate`
- `cargo fmt --check`
- `git diff --check`
- `cargo test -p tx-ext4-format --test pager_mock xattr_ -- --nocapture`
