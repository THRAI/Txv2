# RV64 large-clone timer leak and Git workflow recovery

Date: 2026-08-01  
Branch: `feature-network-refactor`  
Starting HEAD: `65209fe3`

## Outcome

The requested guest workflow now gets through the kernel-sensitive parts:

- a fresh clone of `https://github.com/oscomp/xv6-riscv.git` receives all
  7,780 objects (17.46 MiB) and resolves all 4,127 deltas without a panic;
- `echo "hello" > README` persists six bytes on the ext4 sdcard;
- a new process reads `hello` back;
- `git add .` and `git commit -m"ddd"` succeed, producing guest commit
  `c49e801` with one insertion instead of an empty README;
- HTTPS push authenticates and reaches GitHub, where normal non-fast-forward
  protection rejects it because `me/riscv` already has commits not in the
  fresh xv6 clone;
- `git pull me riscv` fetches successfully, then stops because modern Git
  requires an explicit merge, rebase, or fast-forward-only policy for the
  divergent branches.

No force push was attempted and the remote branch was not modified. Choosing a
pull reconciliation policy is repository-history policy, not a remaining
kernel defect.

The final serial log is:

```text
target/git-xv6-full-workflow-after-fsync-restoration-20260801.log
```

## Failure 1: large-clone allocation panic

The first saved reproduction reached 76% of the clone (5,972 of 7,780 objects,
15.16 MiB) and failed a 2,621,440-byte allocation. At failure, 234,762 of
262,144 pages were free, but the largest contiguous run was only 461 pages
while the request needed 640 pages. This was contiguous-run exhaustion, not
total physical OOM.

The temporary allocator diagnostic recorded caller `0xffffffff8054f810`.
Decoding it with the ordinary release ELF:

```sh
cargo xtask fault-decode --target rv64-qemu \
  --elf target/riscv64gc-unknown-none-elf/release/tx-kernel-riscv64-qemu-virt \
  --addr 0xffffffff8054f810
```

resolved to `alloc::raw_vec::RawVecInner::reserve`. Release disassembly reduced
the allocation owner to `tx_time::TimerEngine::insert`; the exact request is:

```text
40-byte Slot * 65,536 capacity = 2,621,440 bytes
```

`MinHeap::insert` appended a new `Slot` for every timer. Cancellation and
expiry removed the live heap entry but only set `Slot.live = false`; neither
path deleted the key-index entry nor reused the physical slot. The network
delegate clamps its next wake to at most 10 ms, creating and dropping roughly
100 guards per second. Reaching 32,768 historical slots therefore takes about:

```text
32,768 * 10 ms = 327.68 seconds
```

This matches the roughly five-and-a-half-minute panic. The timer heap entered
through merge-side commit `dd9435f3`; it does not exist at premerge anchor
`90939012`. The large clone is only the duration trigger. Increasing a thread
stack, Git buffer, TCP buffer, or QEMU RAM would not fix the monotonic registry.

## Timer repair

`crates/tx-time/src/timer/min_heap.rs` now:

- stores an intrusive free-list link in dead slots and reuses them on insert;
- recycles slots on both cancellation and expiry;
- enforces capacity against the live heap length rather than historical slot
  vector length;
- removes open-addressed key-index entries with backward-shift cluster repair,
  preserving lookup across wrapped probe clusters;
- keeps `TimerKey` allocation monotonic, so recycling storage does not recycle
  observable timer identities.

New deterministic tests churn 70,000 canceled timers and 70,000 expired timers
while asserting that one-at-a-time workloads retain one physical slot. A third
test covers deletion from a wrapped key-index probe chain.

## Failure 2: shell redirection produced an empty ext4 file

After the timer repair, the full clone completed, exposing a separate exact
premerge regression:

```text
echo hello > WRITE_TEST       -> command returned 0, file size 0
busybox echo hello > file     -> command returned 0, file size 0
dd if=/tmp/six-bytes of=file  -> file size 6
```

The write path was functional; `dd` explicitly closed its fd and current
`close(2)` invoked page-backed writeback. Shell builtin/applet redirection
instead lost the last file reference through process teardown or through
`dup3` replacing the redirected fd while restoring stdout. Those paths drained
or replaced the `OpenFile` without calling `step_fsync`, so bootstrap ext4
(which has no background writeback planner) retained a create-time zero inode
size.

This is the same issue recorded in the 2026-07-16 progress entry and fixed at
premerge anchor `90939012`. The merge dropped two small pieces:

1. after `drain_fds()` in both `step_exit_group` and final-thread
   `step_process_exit`, deduplicate page-backed open-file descriptions and
   perform best-effort synchronous `step_fsync`;
2. after a successful `dup3` replacement, submit the displaced page-backed
   file through the same close-writeback helper used by `close(2)`.

The restored implementation deliberately does not expand into the deferred
Phase 1 unified OFD-release project. It covers only the two proven premerge
bypasses needed by this workflow.

## Verification

Host and build evidence:

```text
tx-time debug:                         35/35
tx-time release:                       35/35
tx-reactor integration targets:        all pass
tx-shims fd_ops_wave2:                 46/46
new exit_group/final-thread fsync:     2/2
RV64 release kernel build:             pass
git diff --check:                      pass
```

The two process-exit tests and the dup3 test were run before the implementation
and each failed with an observed fsync count of zero; all three pass after the
repair.

The broader process unit filter passes 124 of 126 tests. Its two failures are
pre-existing and unrelated: one source-string invariant still searches the old
outer `step_exit_group_with_posts` body, and one double fatal-group-exit test
expects a terminating signal overwrite that current code does not perform.
The repository-wide `cargo -q xtask unit` baseline also remains blocked by the
previously recorded three tx-shims failures and eight stale tx-ext4
`with_target` compile errors.

Progress/document hygiene gates retain unrelated repository debt:
`cargo xtask progress validate` rejects the pre-existing
`2026-07-24-network-time-integration.json` status spelling `completed`, and
`cargo xtask lint docs` reports the existing active-doc anchor/link backlog (23
broken links). The two changed progress documents introduce no Markdown link
targets, and `git diff --check` passes.

Guest evidence after both repairs:

```text
clone:       7780/7780 objects, 4127/4127 deltas
README:      6 bytes, content "hello"
commit:      c49e801 ddd, 1 insertion / 48 deletions
push:        authenticated HTTPS; rejected non-fast-forward
pull:        fetched me/riscv; stopped for reconciliation policy
final cat:   hello
```

All temporary allocator caller and in-flight syscall diagnostics were removed
before the final release build. The resulting tree contains only the timer
repair, the two premerge file-flush restorations, their tests, and progress
records.

## Remaining decision

To make the remote push succeed, the user must choose how the unrelated or
divergent `riscv` histories should be reconciled:

- merge: `git pull --no-rebase me riscv`;
- rebase: `git pull --rebase me riscv`;
- fast-forward only: `git pull --ff-only me riscv` (expected to reject while
  the local commit diverges).

After a successful merge or rebase and conflict review, an ordinary
`git push me riscv` can be attempted. Force push is outside this recovery and
was not used.
