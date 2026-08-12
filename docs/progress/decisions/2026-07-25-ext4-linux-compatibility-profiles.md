# Ext4 Linux Compatibility Profiles

Date: 2026-07-25

## Context

Tx already has a native ext4 format crate, mounted filesystem instance,
PageBacked integration, I/O-manager planner path, JBD2 replay and immutable
mutation-admission work. Moving ext4 behind a userspace daemon or a private
in-kernel service would not remove the format, allocation, namespace or crash
consistency work. It would add request transport, page transfer, credentials,
daemon/service failure, bootstrap and recovery protocols.

The previous operational plan was phrased as a full rsext4 migration. That made
an implementation reference sound like the compatibility authority and did not
separate a bounded production milestone from mainstream Linux compatibility.

## Decision

1. Production ext4 remains a Tx-native mounted kernel filesystem instance. It
   is not moved behind a userspace daemon or private ext4 service transport.
2. Generic PageBacked/I/O-manager service futures remain the asynchronous
   execution mechanism. ext4 owns L5 mapping, allocation and journal planning;
   it does not own a private scheduler or second page cache.
3. Linux ext4 on-disk documentation, Linux-visible behavior, e2fsprogs and
   xfstests are the compatibility authorities. rsext4 is an algorithm reference
   only and is never a runtime dependency.
4. Delivery has two cumulative gates:
   - Tier 1 is a pinned 4-KiB controlled profile with bounded extent/htree
     shapes, the common namespace/data/metadata operations, JBD2 ordered-mode
     durability, classic-orphan recovery, Alpine/OSComp witnesses and
     deterministic crash/e2fsck proof.
   - Tier 2 removes the shape bounds and adds mainstream extent/htree, orphan,
     xattr/ACL, fallocate, coherency, mount-error and xfstests coverage.
5. Tier 3 features are rejected explicitly. A new feature enters Tier 1 or 2
   only with a capability-ledger row, deterministic fixture, feature-admission
   rule, Linux/e2fsprogs oracle and crash policy.

## Rejected alternatives

- **Userspace ext4 daemon:** rejected as the primary path because Tx would first
  need a FUSE-like transport and still implement or import a complete ext4
  engine. It may be reconsidered as a separate FUSE feature, not as tx-ext4.
- **Private in-kernel ext4 service:** rejected because generic L4/L6 service
  futures already provide scheduling and waiting. A second request protocol
  would duplicate ownership without fault isolation.
- **Full rsext4 migration:** rejected as a goal. Individual algorithms may be
  adapted after Tx ownership and Linux/e2fsprogs oracle rows are explicit.
- **Immediate full ext4 parity:** rejected because it makes the first production
  gate unbounded and prevents honest feature admission.

## Consequences

- `docs/design/05_filesystem/TX_EXT4_PLAN_v1_2.md` is the canonical contract and
  now defines the authority stack, feature admission, Tier 1/Tier 2 surfaces,
  phased gates and explicit Tier 3 exclusions.
- `docs/superpowers/plans/2026-07-23-rsext4-full-migration.md` and its JSON
  record retain their stable filenames but now track Linux compatibility
  convergence. Their old rsext4 wording is historical, not architectural.
- Tier 1 production cutover remains blocked while direct pager writes and the
  journal planner/runtime are both reachable, publication can precede commit
  completion, or the feature/profile manifest is absent.
- Tier 2 work starts only after Tier 1 remains green as a permanent fast gate.

## Verification and next step

This decision changes documentation and plan state only. Required closeout is
`cargo xtask lint docs`, `cargo xtask progress validate` and
`git diff --check`. The next implementation step is to make the Tier 1 feature
mask and deterministic fixture manifest executable, then close
publication-after-commit for the SetAttr vertical slice.
