# Branch and Disk Cleanup Inventory

Date: 2026-07-30

## Scope

This began as a read-only inventory of local branches, Git worktrees, Git
storage, Cargo output, OSComp images, and nearby shared Rust caches. After the
user explicitly approved a retention policy, 53 old non-empty LTP runtime
images were deleted. No fetch, branch deletion, worktree removal, `git gc`,
`git clean`, `cargo clean`, source edit, base-image deletion, or test-evidence
deletion was performed.

## Git Snapshot

- Current branch: `feature-network-refactor` at `add3faec5fda`; the main
  worktree was clean before this progress catch-up.
- The current branch is 34 commits ahead and 0 behind its locally cached
  upstream, and 22 commits ahead of local `main`.
- Local refs contain 21 branches. Cached remote refs contain 37 actual tracking
  branches plus `origin/HEAD`.
- There are seven registered worktrees. Four clean auxiliary worktrees are
  potential removal candidates because each detached HEAD is also protected by
  a local branch:
  - `/home/msp/learning/Txv2-premerge-feature`
  - `/home/msp/learning/Txv2-premerge-git`
  - `.claude/worktrees/heuristic-swartz-0ab0c2`
  - `.claude/worktrees/objective-boyd-1aad05`
- Do not remove `/home/msp/learning/Txv2-main-baseline` or
  `.claude/worktrees/ipv6-external` without preserving their uncommitted files.
- Seven non-current local branch refs are strict ancestors of the current
  branch. Twelve other local branches are not ancestors of either the current
  branch or `main`; backup-looking names alone are not evidence that those refs
  are disposable.
- Remote counts and ahead/behind values are local-cache observations. No fetch
  was performed, and `origin/main` last moved locally on 2026-07-26.

## Disk Snapshot

The filesystem was 80% used, with about 179 GiB available at inventory time.
The repository occupied at least 259.17 GiB physically; five unreadable
`nobody:nogroup` LTP work directories make this a lower bound.

| Surface | Physical size | Treatment |
| --- | ---: | --- |
| `target/` | 235.69 GiB | Main project-space source |
| `target/oscomp/` | 191.77 GiB | Mostly per-run writable disk copies |
| `target/oscomp/{ltp-bin,ltp-runtest}/*.img` | 185.01 GiB | Strongest regeneration-only cleanup candidate |
| `target/oscomp/testdata/` | 6.46 GiB | Preserve as base images |
| Main-worktree Cargo outputs | about 43.63 GiB | Regenerable, but expensive to rebuild |
| Nested `.claude` worktrees | 19.07 GiB | Almost entirely per-worktree `target/` |
| All six auxiliary worktree `target/` directories | 32.47 GiB | Separate from branch refs |
| `local-images/` | 4.01 GiB | Active local image inputs; preserve by default |
| `.git/` | 0.29 GiB | Low-value cleanup target |
| `msp/` | 0.04 GiB | Preserve reports and debug evidence |
| Shared `/home/msp/.cargo` | 0.21 GiB | Not material |
| Shared `/home/msp/.rustup` | 12 GiB | Machine-wide toolchains, not project-local |

The two witness runners copy a base disk into a per-run image and leave that
image behind:

- `tools/ltp-bin-witness.sh`
- `tools/ltp-runtest-witness.sh`

There are 618 matching runtime image paths, of which 61 have allocated blocks.
Their combined allocated size is 198,656,012,288 bytes (185.01 GiB). Logs and
judge outputs in the same directories occupy only about 23 MiB and can be
preserved independently.

## Cleanup Boundaries

1. Highest-confidence first action: after explicit approval and a fresh QEMU
   process check, remove only the runtime `.img` files directly under
   `target/oscomp/ltp-bin/` and `target/oscomp/ltp-runtest/`. Keep
   `target/oscomp/testdata/`, logs, judges, kernels, and submit artifacts.
   Expected recovery: about 185.01 GiB.
2. Optional worktree action: remove the four clean auxiliary worktrees with
   `git worktree remove`, not a raw recursive delete. Expected recovery: about
   17.71 GiB. This does not require deleting their protecting branch refs.
3. Optional compile-cache action: selectively remove Cargo-owned build
   directories. Do not use a broad repository-root `cargo clean`, because this
   project also stores OSComp images, logs, and test evidence below `target/`.
4. Branch deletion is organizational cleanup, not meaningful disk cleanup.
   Do not delete the twelve unmerged refs until patch/content equivalence and
   current remote state have been checked.

## Executed Cleanup

The approved policy retained the newest non-empty runtime image in each
`runner mode x architecture/libc lane` group. The retained images are:

- `target/oscomp/ltp-bin/sd-fanoutsolo-rv.musl.img`
- `target/oscomp/ltp-bin/sd-fanoutsolo-rv.glibc.img`
- `target/oscomp/ltp-bin/sd-laD-la.musl.img`
- `target/oscomp/ltp-bin/sd-laD-la.glibc.img`
- `target/oscomp/ltp-runtest/sd-baseline-rv.musl.img`
- `target/oscomp/ltp-runtest/sd-netbase2-rv.glibc.img`
- `target/oscomp/ltp-runtest/sd-labase-la.musl.img`
- `target/oscomp/ltp-runtest/sd-labase-la.glibc.img`

The cleanup deleted the other 53 non-empty runtime images and reclaimed
172,082,016,256 bytes (160.26 GiB). It deliberately left 557 zero-byte image
placeholders unchanged.

Post-cleanup checks established:

- non-empty runtime image count: `61 -> 8`;
- `target/oscomp`: `205,911,654,400 -> 33,829,638,144` bytes;
- visible repository lower bound: `106,202,001,408` bytes (98.91 GiB);
- `target/`: `80,985,088,000` bytes (75.42 GiB);
- filesystem usage: `80% -> 62%`;
- filesystem available space: about `179 GiB -> 339 GiB`;
- `.log` plus `.judge` count remained `1251`;
- both `target/oscomp/testdata/sdcard-{rv,la}.img` stat identity/size/block/mtime
  records remained unchanged;
- all eight retained runtime images remained present and non-empty.

The deleted runtime disks are not recoverable in place, but they are
regeneration-only artifacts: the witness scripts recreate them from the
preserved `testdata` base images. The corresponding logs and judge outputs
remain available.

## Verification and Next Step

Read-only verification used `git status`, `git for-each-ref`,
`git rev-list`, `git worktree list --porcelain`, `git count-objects -vH`,
`du`, `find`, `stat`, and `df`. The destructive step additionally asserted:
no matching QEMU process, exactly 61 non-empty candidates, exactly eight
explicit retained paths, exactly 53 deletion paths, and exactly
172,082,016,256 allocated bytes in the deletion set.

The next optional action is either selective Cargo-cache cleanup or removal of
the four clean auxiliary worktrees. The only inventory limitation is the five
unreadable LTP work directories; their size remains unknown.
