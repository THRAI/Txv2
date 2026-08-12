# rsext4 reference inventory for Tx-native ext4

Date: 2026-07-23

## Scope

This inventory maps potentially reusable algorithms from the pinned
`external/rsext4` implementation onto the current tx-ext4 architecture. It is
evidence for `docs/progress/plans/2026-07-23-rsext4-full-migration.json`, not an
alternative filesystem design or compatibility authority.
`TX_EXT4_PLAN_v1_2.md` remains authoritative; Linux ext4, e2fsprogs and
xfstests define compatibility.

The inventory is complete when every relevant rsext4 algorithm has a Tx owner,
oracle and disposition. Product completeness is different: Tier 1 closes the
controlled profile; Tier 2 closes the declared mainstream Linux surface even
where rsext4 has no matching operation. Quotas, encryption, verity, online
resize, non-4-KiB blocks, ext2/ext3 compatibility, reflink and journal data or
writeback modes remain Tier 3. Common xattrs and POSIX ACLs are Tier 2.

## Capability map

| Capability | rsext4 evidence | Current Tx evidence | Migration disposition |
|---|---|---|---|
| Superblock, GDT, inode, checksum | `superblock.rs`, `blockgroup_description.rs`, `disknode.rs`, `checksum.rs` | `tx-ext4-format/src/ondisk.rs` | Audit field and feature-mask parity; keep Sans-IO Tx encoders. |
| Mount and feature validation | `ext4.rs:279` | `Ext4Pager::open` and Tx mount runtime | Rewrite policy in Tx; rsext4's unconditional feature acceptance is inadmissible. |
| Linear directory and htree lookup | `dir.rs:146`, `hashtree.rs:436` | directory/dx parsers and `Ext4Pager::lookup` | Port algorithms and image witnesses, not caches. |
| Extent lookup, insert, remove, split | `extents_tree.rs:303`, `extents_tree.rs:883` | read mapping plus limited root insertion | Port as mutation-plan builders with allocation claims and after-images. |
| Block/inode allocation and free | `ext4.rs:828-1178`, `bmalloc.rs` | allocation helpers; reclaim incomplete | Port selection/checksum logic into reserve-then-commit transactions. |
| Regular create/read/write/truncate | `file.rs:81`, `file.rs:1584`, `file.rs:1838` | create/read/basic writes; hole/truncate durability incomplete | Complete through neutral planner and ordered metadata transaction. |
| mkdir/rmdir/rename | `dir.rs`, `file.rs:23`, `file.rs:536` | basic methods exist but are not fully atomic/reclaiming | Rebuild as multi-block atomic mutation plans; do not port path orchestration. |
| hard link, symlink, readlink | `file.rs:249`, `file.rs:924` | link/symlink are `ENOSYS`; read-link format exists | Port inode/dir-entry mechanics; use FsObjectId APIs and VFS policy. |
| JBD2 encode, commit, replay | `jbd2/jbd2.rs`, `jbd2/jbdstruct.rs` | transaction image/replay and graph scheduler foundations | Reimplement ordered commit, revoke, wrap, checkpoint and barriers as StepOp/graphs. |
| chmod/chown/timestamps | inode fields only; no rsext4 operation | VFS traits/syscalls exist; ext4 inherits `ENOSYS` | Tx-native metadata mutation; first production slice. |
| xattr/ACL/quota/encryption | fields/constants only | xattr/ACL are Tier 2; quota/encryption are Tier 3 | Design and implement common xattr/ACL storage against Linux/e2fsprogs; do not treat rsext4 constants as semantics. Reject quota/encryption combinations explicitly. |

## Code that must not be ported directly

- `Ext4FileSystem` inode, bitmap and data-block caches. PageContainer and the
  I/O manager own Tx cache and in-flight state.
- Synchronous `&mut Jbd2Dev<B>` chains and direct cache writeback. Tx I/O yields
  through typed operations and completion waits.
- Path-string namespace functions. VFS resolves paths and passes parent/object
  IDs.
- Mount-time creation of root, lost+found or a missing journal. Tx rejects or
  recovers according to an explicit feature/state policy.
- `is_all_feature_supported() -> true`, ignored checksum failures, panics and
  best-effort deletion. Corrupt or unsupported images get deterministic errors.
- rsext4 journal ordering as proof of durability. Tx represents data,
  descriptor, metadata, commit, barrier, checkpoint and fsync dependencies.

## Readiness conclusion

Ready: mostly for Tier 1; not ready for Tier 2 closure.

The ownership boundary, format/runtime split, mutation IR, planner graph, VFS
traits and production mount path exist. The first metadata-only slice can start.
Tier 1 is blocked by publication-after-commit, one production durability path,
the exact feature/profile admission table, classic-orphan recovery and
executable crash/interoperability witnesses. Tier 2 is additionally blocked by
arbitrary-depth extent and htree mutation, `orphan_file`/high-concurrency orphan
recovery, common xattr/ACL/fallocate/direct-I/O semantics and a versioned
xfstests manifest.

Implementation entry order:

1. feature matrix and image oracle;
2. mutation transaction core;
3. chmod/chown/time inode metadata;
4. hard link and symlink;
5. allocation/free, extents and truncate;
6. atomic namespace and orphan handling;
7. ordered JBD2 durability and recovery;
8. Tier 1 production cutover, profile/crash/interoperability gate;
9. Tier 2 advanced shapes, metadata/coherency features and xfstests closure.
