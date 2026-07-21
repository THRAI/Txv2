# EBR and vmalloc mature implementation shape

Date: 2026-07-21

## External references

- Crossbeam Epoch's current `internal.rs` uses a 64-callback thread-local bag,
  seals it after a SeqCst fence with the current epoch, collects at most eight
  sealed bags per pass, and enters the cold collector every 128 pinnings:
  <https://github.com/crossbeam-rs/crossbeam/blob/main/crossbeam-epoch/src/internal.rs>
- Linux `kvmalloc` first attempts physically contiguous allocation and falls
  back to vmalloc, while avoiding disruptive retry/reclaim for large attempts:
  <https://github.com/torvalds/linux/blob/master/mm/slub.c>
- Linux documents virtually contiguous allocation and lazy/batched vmap TLB
  invalidation:
  <https://docs.kernel.org/core-api/mm-api.html>
- Linux's cache/TLB documentation requires page-table changes to be visible
  before old backing can be reused and provides range operations to avoid one
  flush per page:
  <https://docs.kernel.org/next/core-api/cachetlb.html>

## Txv2 adaptation

- EBR now keeps one inline 64-entry bag per CPU and publishes full/explicitly
  flushed bags to a growable page-backed FIFO. It has no fixed retired-object
  ceiling. Collection is bounded to eight bags, periodic collection does not
  flush partial local bags, reclaim callbacks execute outside the queue lock,
  and empty-page caching is capped at 64 bags.
- Txv2's current frame allocator is a bitmap rather than a buddy allocator, so
  a multi-megabyte contiguous attempt cannot be made cheap with a no-reclaim
  GFP policy. Requests up to 1 MiB use direct-map allocation first and fall
  back to vmalloc; larger requests use vmalloc first but still retain direct
  allocation as a fallback if vmalloc cannot represent the request.
- New vmalloc PTEs are populated without a per-page TLB operation and followed
  by one range publication. Freeing clears the full range, issues one batched
  shootdown, then releases physical frames through an intrusive temporary chain
  stored in the dead object pages. This avoids allocator recursion during
  `vfree` and enforces `unmap -> shootdown -> frame reuse`.

## Remaining performance boundary

The implementation still writes one 4 KiB PTE and allocates one physical frame
at a time. PMD/2 MiB mappings and per-CPU vmap-area caches would be later
optimizations; they are not required to remove the observed per-page global TLB
flush storm or the 6 MiB contiguous-frame requirement.
