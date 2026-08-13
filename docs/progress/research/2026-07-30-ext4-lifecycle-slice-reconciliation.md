# ext4 Lifecycle Slice Reconciliation

Date: 2026-07-30

Current `HEAD` (`101b98daec2fc1746d883d15715d2093e3ca850f`) is the
documentation-only lifecycle execution ledger. The baseline format, ext4, and
PageBacked test commands named by Task 1 passed before this review.

| commit | verified invariant | tests worth importing | code disposition | target task |
|---|---|---|---|---|
| `1cee426a` | Metadata-csum inode after-images preserve unknown bytes and recompute checksums, including high bits. | Host checksum and pager mock after-image cases. | Reapply only checksum helpers against current ondisk APIs. | Task 10/12 |
| `9f39f58f` | Namespace visibility waits for checkpoint, rather than publishing after commit. | Namespace create/unlink publication-order cases. | Express through `MutationHandle` terminal settlement, not its callback topology. | Task 5/12 |
| `555e4a40` | Replay requires `needs_recovery`; checkpoint refreshes pager/bridge-visible caches; PageBacked wakes checkpoint successors. | Recovery, device, cache-refresh, and PageBacked error-path cases. | Split by lifecycle owner; never import the 17-file patch wholesale. | Task 3/4/7/8 |

The historical patches were inspected with `git show --stat`; none was
cherry-picked. Their listed tests are retained as future RED witnesses, while
the new owner boundaries in `EXT4_LIFECYCLE_v1.md` determine the implementation
shape.
