# LTP vm Progress

`vm` batch local tracking. Cases are from `tools/ltp-batches.py --batch vm`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 103 | from `make ltp-batch-cases LTP_BATCH=vm` |
| latest local run | focused full-image tail | 2026-05-30 direct QEMU tail covered `munlockall01..set_mempolicy04` |
| cumulative scored | `113/253` | recorded rows in this document |
| 2026-05-30 full-image partial run | `83/210` | completed 87/88 started cases; serial `target/oscomp/os_serial_out_ltp_vm_partial_20260530_170854.txt` |
| 2026-05-30 full-image tail run | `13/22` | completed all 15 selected suffix cases; serial `target/oscomp/os_serial_out_ltp_vm_tail_20260530_200054.txt` |
| reached case | `set_mempolicy04` | batch completed |
| logs | `target/oscomp/ltp-progress/vm` | per-group stdout and serial snapshots |

2026-05-30 focused full-image tail command:

```bash
timeout 180s cargo xtask oscomp qemu --target rv64-qemu \
  --data target/oscomp/testdata --submit target/oscomp/submit \
  --suite ltp-musl:munlockall01+munmap01+munmap02+munmap03+pkey01+process_madvise01+remap_file_pages01+remap_file_pages02+sbrk01+sbrk02+sbrk03+set_mempolicy01+set_mempolicy02+set_mempolicy03+set_mempolicy04
python3 tools/oscomp-judge.py \
  target/oscomp/os_serial_out_ltp_vm_tail_20260530_200054.txt \
  target/oscomp/testdata
cargo xtask fault-decode --target rv64-qemu \
  --serial target/oscomp/os_serial_out_ltp_vm_tail_20260530_200054.txt \
  --all --brief
```

The focused suffix completed without kernel trap lines. It confirms
`munlockall01`, `munmap03`, `remap_file_pages01`, `remap_file_pages02`, and
`sbrk02` pass. `munmap01` and `munmap02` exit 139 without a kernel trap marker,
so they need userspace-visible fault/exit triage. Remaining tail skips are
expected pkey, process_madvise, sbrk03 arch filtering, and libnuma-gated
`set_mempolicy*`; `sbrk01` still reports `ENOMEM` on grow/shrink subcases.

2026-05-30 direct full-image run command:

```bash
timeout 300s cargo xtask oscomp qemu --target rv64-qemu \
  --data target/oscomp/testdata --submit target/oscomp/submit \
  --suite ltp-batch:vm
python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata
```

The fresh partial run confirms the brk, basic mlock/mlockall, many mmap,
mremap, msync, and munlock happy paths still execute on the full image. The
main remaining clusters are unsupported NUMA/mempolicy cases, partial `madvise`
coverage, mincore/mlock residency accounting, mmap/mprotect SIGSEGV semantics,
and `msync`/`munlock` errno validation.

## 2026-05-26 failure notes

- Low-cost fix batch: wired RV64 `mincore`/`mlockall`/`munlockall`/`mlock2`, added `/proc/<pid>/status` `VmLck`, made `/proc/self` resolve to the active OSComp test process in local single-hart runs, split VM recipes for exact locked ranges, fixed RISC-V `PROT_WRITE` PTEs to include read permission, and enforced `RLIMIT_MEMLOCK` for non-root callers.
- Procfs follow-up: added `/proc/<pid>/smaps` projection for VMA `Rss`/`Locked`; `mlock05` now passes on RV64 and LA64. Current `/proc/self/maps` behavior also makes `mmap04` pass on both architectures.
- `remap_file_pages02`: added syscall #234 dispatch with unsupported nonlinear mappings returning `EINVAL` for concrete calls while preserving the all-zero LTP feature probe as `ENOSYS`; RV/LA now pass the four invalid-argument checks.
- `mmap` errno follow-up: file-backed mappings now require a readable fd (`EACCES` for write-only fds), and `MAP_SHARED_VALIDATE` returns `EOPNOTSUPP` for unknown flag bits; `mmap06`/`mmap20` now pass on RV/LA.
- `mmap08`: file-backed mappings now report `EBADF` for invalid fds before zero-length validation; RV/LA pass.
- `mremap03`: missing old mapping ranges now return `EFAULT` instead of `EINVAL`; RV/LA pass.
- Remaining mincore gap: `mincore02`/`mincore03` enter the syscall but touched anonymous pages still report non-resident because pmap residency is not yet reflected for those user faults.
- Remaining mlock gap: `mlock201` `MLOCK_ONFAULT` cases pass, but plain `mlock2(flags=0)` does not eagerly fault non-present pages, so mincore still sees 0 present pages for those subcases.
- TCONF: 24 recorded case(s); see per-case notes below.
- TFAIL: 16 recorded case(s); see per-case notes below.
- TBROK: 14 recorded case(s); see per-case notes below.
- `TCONF`: 13 recorded case(s); see per-case notes below.
- EINVAL observed: 3 recorded case(s); see per-case notes below.
- syscall variant passes; libc variant reports `TCONF: 2 recorded case(s); see per-case notes below.
- `MADV_HWPOISON` returns `ENOSYS`: 1 recorded case(s); see per-case notes below.
- `splice failed`: 1 recorded case(s); see per-case notes below.
- `tst_checkpoint_wait(0, 10000)` times out: 1 recorded case(s); see per-case notes below.
- basic advice values pass; many others return `ENOSYS`: 1 recorded case(s); see per-case notes below.
- cannot resolve tmp shmem path, checkpoint wait times out, then guest does not exit after group end: 1 recorded case(s); see per-case notes below.
- invalid/edge cases have errno mismatches or unexpectedly succeed: 1 recorded case(s); see per-case notes below.
- issue not reproduced: 1 recorded case(s); see per-case notes below.
- missing `/proc/self/mounts` and cgroup/proc sysctl setup: 1 recorded case(s); see per-case notes below.
- no zero-fill-on-demand pages observed for anonymous private mappings: 1 recorded case(s); see per-case notes below.
- normal advice passes; `MADV_WIPEONFORK`/`MADV_KEEPONFORK` return `ENOSYS`: 1 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `brk01` | 1/2 | partial | syscall variant passes; libc variant reports `TCONF: brk() not implemented` |
| `brk02` | 1/2 | partial | syscall variant passes; libc variant reports `TCONF: brk() not implemented` |
| `dirtyc0w` | 0/1 | fail | single-case rerun exits cleanly; `tst_checkpoint_wait(0, 10000)` times out |
| `dirtyc0w_shmem` | 0/1 | fail | prints FAIL and group end, but guest does not reach `userspace:exited`; external timeout kills QEMU |
| `dirtypipe` | 0/1 | fail | `splice failed` |
| `get_mempolicy01` | 0/1 | skip | `TCONF`: requires libnuma development packages with `LIBNUMA_API_VERSION >= 2` |
| `get_mempolicy02` | 0/1 | skip | `TCONF`: requires libnuma development packages with `LIBNUMA_API_VERSION >= 2` |
| `madvise01` | 6/20 | partial | basic advice values pass; many others return `ENOSYS` |
| `madvise02` | 1/13 | partial | invalid/edge cases have errno mismatches or unexpectedly succeed |
| `madvise03` | 0/1 | fail | no zero-fill-on-demand pages observed for anonymous private mappings |
| `madvise05` | 1/1 | pass | issue not reproduced |
| `madvise06` | 0/1 | fail | missing `/proc/self/mounts` and cgroup/proc sysctl setup |
| `madvise07` | 0/1 | fail | `MADV_HWPOISON` returns `ENOSYS` |
| `madvise08` | 0/1 | skip | `TCONF`: missing `/proc/sys/kernel/core_pattern` |
| `madvise09` | 0/1 | skip | `TCONF`: missing `/sys/fs/cgroup/memory`, likely no `CONFIG_MEMCG` surface |
| `madvise10` | 2/6 | partial | normal advice passes; `MADV_WIPEONFORK`/`MADV_KEEPONFORK` return `ENOSYS` |
| `madvise11` | 0/1 | skip | `TCONF`: `CONFIG_MEMORY_FAILURE` missing in `/proc/config` |
| `mbind01` | 0/1 | skip | `TCONF`: requires libnuma development packages with `LIBNUMA_API_VERSION >= 2` |
| `mbind02` | 0/1 | skip | `TCONF`: requires libnuma development packages with `LIBNUMA_API_VERSION >= 2` |
| `mbind03` | 0/1 | skip | `TCONF`: requires libnuma development packages with `LIBNUMA_API_VERSION >= 2` |
| `mbind04` | 0/1 | skip | `TCONF`: requires libnuma development packages with `LIBNUMA_API_VERSION >= 2` |
| `memfd_create01` | 0/1 | skip | `TCONF`: syscall `279` / `__NR_memfd_create` not supported |
| `memfd_create02` | 0/1 | skip | `TCONF`: syscall `279` / `__NR_memfd_create` not supported |
| `memfd_create03` | 0/1 | skip | `TCONF`: `hugetlbfs` is not supported |
| `memfd_create04` | 0/1 | skip | `TCONF`: huge page is not supported |
| `migrate_pages01` | 0/2 | skip | TCONF: migrate_pages01.c:250: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `migrate_pages02` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `migrate_pages03` | 0/1 | skip | TCONF: require libnuma >= 2 and it's development packages |
| `mincore01` | 4/4 | pass | fixed RV64 syscall 232 collision and added mincore EINVAL/EFAULT/ENOMEM handling |
| `mincore02` | 1/2 | partial | syscall succeeds; residency count remains 0 because touched anonymous pages are not visible through current pmap mincore observation |
| `mincore03` | 1/2 | partial | untouched pages correctly report non-resident; touched pages still report 0 resident instead of 3 |
| `mincore04` | 0/1 | fail | single-case rerun exits cleanly; TBROK: `tst_checkpoint_wait(0, 10000)` failed: ETIMEDOUT (110) |
| `mlock01` | 4/4 | pass |  |
| `mlock02` | 3/3 | pass | missing mappings now return ENOMEM and non-root RLIMIT_MEMLOCK zero returns EPERM |
| `mlock03` | 1/1 | pass | EINVAL observed |
| `mlock04` | 1/1 | pass |  |
| `mlock05` | 2/2 | pass | `/proc/self/smaps` now reports matching `Rss` and `Locked` for mlocked anonymous mappings; RV/LA pass |
| `mlock201` | 4/8 | partial | `MLOCK_ONFAULT` subcases pass; plain `mlock2(flags=0)` still does not eagerly fault non-present pages so mincore reports 0 present pages |
| `mlock202` | 4/4 | pass | `mlock2` syscall wired; invalid flag, RLIMIT_MEMLOCK ENOMEM/EPERM, and unmapped-range ENOMEM pass |
| `mlock203` | 1/1 | pass | `/proc/self/status` `VmLck` now works and repeated lock does not increase locked count |
| `mlockall01` | 3/3 | pass | valid `MCL_CURRENT`, `MCL_FUTURE`, and combined flags accepted |
| `mlockall02` | 1/3 | partial | invalid flag branch passes; two BEHAVE probes remain TCONF |
| `mlockall03` | 3/3 | pass | RLIMIT_MEMLOCK ENOMEM/EPERM and unknown flag EINVAL pass |
| `mmap01` | 1/1 | pass |  |
| `mmap02` | 1/1 | pass |  |
| `mmap03` | 0/1 | fail |  |
| `mmap04` | 14/14 | pass | `/proc/self/maps` reports expected mapping permissions; RV/LA pass |
| `mmap05` | 0/1 | fail | TBROK: Test killed by SIGSEGV! |
| `mmap06` | 8/8 | pass | write-only file-backed mappings now return `EACCES`; RV/LA pass |
| `mmap08` | 1/1 | pass | invalid file-backed fd now returns `EBADF` before zero-length `EINVAL`; RV/LA pass |
| `mmap09` | 3/3 | pass |  |
| `mmap12` | 0/1 | fail | TFAIL: pen dev pagemap failed: ENOENT (2) |
| `mmap13` | 0/1 | fail | TBROK: Test killed by SIGSEGV! |
| `mmap14` | 0/1 | fail | TFAIL: mmap14.c:100: Open dev status failed: errno=ENOENT(2): No such file or directory |
| `mmap15` | 1/1 | pass | EINVAL observed |
| `mmap16` | 0/1 | skip | TCONF: Couldn't find 'mkfs.ext4' in $PATH |
| `mmap17` | 1/1 | pass |  |
| `mmap18` | 0/8 | fail | TBROK: mmap(0x109000,4096,PROT_READ / PROT_WRITE(3),306,-1,0) failed: EINVAL (22) |
| `mmap19` | 1/1 | pass |  |
| `mmap20` | 1/1 | pass | `MAP_SHARED_VALIDATE` with unknown flag returns `EOPNOTSUPP`; RV/LA pass |
| `move_pages01` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages02` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages03` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages04` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages05` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages06` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages07` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages09` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages10` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages11` | 0/2 | skip | TCONF: move_pages_support.c:411: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `move_pages12` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `mprotect01` | 1/4 | partial | TBROK: mprotect01.c:150: mmap failed |
| `mprotect02` | 0/2 | fail | TBROK: mprotect02.c:102: child exited abnormally with status: 11 |
| `mprotect03` | 1/1 | pass |  |
| `mprotect04` | 0/2 | fail |  |
| `mprotect05` | 1/1 | pass |  |
| `mremap01` | 0/2 | fail | TBROK: mremap01.c:213: writing to mremapfile failed: errno=EINVAL(22): Invalid argument |
| `mremap02` | 1/1 | pass |  |
| `mremap03` | 1/1 | pass | missing old mapping returns `EFAULT`; RV/LA pass |
| `mremap04` | 1/1 | pass |  |
| `mremap05` | 7/7 | pass |  |
| `mremap06` | 3/3 | pass |  |
| `msync01` | 1/1 | pass |  |
| `msync02` | 1/1 | pass |  |
| `msync03` | 2/6 | partial | TFAIL: msync03.c:141: msync succeeded unexpectedly |
| `msync04` | 0/2 | fail | TBROK: Failed to acquire device |
| `munlock01` | 4/4 | pass |  |
| `munlock02` | 1/1 | pass | missing mapping now returns ENOMEM |
| `munlockall01` | 2/2 | pass | `mlockall` raises `VmLck`; `munlockall` clears locked ranges back to 0 |
| `munmap01` | 0/1 | fail |  |
| `munmap02` | 0/1 | fail |  |
| `munmap03` | 3/3 | pass | EINVAL observed |
| `pkey01` | 0/1 | skip | TCONF: syscall(289) __NR_pkey_alloc not supported on your arch |
| `process_madvise01` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `remap_file_pages01` | 2/2 | pass | 2026-05-30 focused tail: current image reports the two scored compatibility checks passing |
| `remap_file_pages02` | 4/4 | pass | invalid argument cases return `EINVAL`; all-zero probe remains `ENOSYS`; RV/LA pass |
| `sbrk01` | 1/3 | partial | TFAIL: sbrk(8192) failed: ENOMEM (12) |
| `sbrk02` | 1/1 | pass |  |
| `sbrk03` | 0/1 | skip | TCONF: This arch 'unknown' is not supported for test! |
| `set_mempolicy01` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `set_mempolicy02` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `set_mempolicy03` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `set_mempolicy04` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
