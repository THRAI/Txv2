# LTP vm Progress

`vm` batch local tracking. Cases are from `tools/ltp-batches.py --batch vm`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 103 | from `make ltp-batch-cases LTP_BATCH=vm` |
| latest local run | VM low-cost fixes | 2026-05-26 targeted reruns for mincore/mlock/munlock |
| cumulative scored | `84/236` | recorded rows in this document |
| reached case | `set_mempolicy04` | batch completed |
| logs | `target/oscomp/ltp-progress/vm` | per-group stdout and serial snapshots |

## 2026-05-26 failure notes

- Low-cost fix batch: wired RV64 `mincore`/`mlockall`/`munlockall`/`mlock2`, added `/proc/<pid>/status` `VmLck`, made `/proc/self` resolve to the active OSComp test process in local single-hart runs, split VM recipes for exact locked ranges, fixed RISC-V `PROT_WRITE` PTEs to include read permission, and enforced `RLIMIT_MEMLOCK` for non-root callers.
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
| `mlock05` | 0/1 | fail | TBROK: fopen(/proc/self/smaps,r) failed: ENOENT (2) |
| `mlock201` | 4/8 | partial | `MLOCK_ONFAULT` subcases pass; plain `mlock2(flags=0)` still does not eagerly fault non-present pages so mincore reports 0 present pages |
| `mlock202` | 4/4 | pass | `mlock2` syscall wired; invalid flag, RLIMIT_MEMLOCK ENOMEM/EPERM, and unmapped-range ENOMEM pass |
| `mlock203` | 1/1 | pass | `/proc/self/status` `VmLck` now works and repeated lock does not increase locked count |
| `mlockall01` | 3/3 | pass | valid `MCL_CURRENT`, `MCL_FUTURE`, and combined flags accepted |
| `mlockall02` | 1/3 | partial | invalid flag branch passes; two BEHAVE probes remain TCONF |
| `mlockall03` | 3/3 | pass | RLIMIT_MEMLOCK ENOMEM/EPERM and unknown flag EINVAL pass |
| `mmap01` | 1/1 | pass |  |
| `mmap02` | 1/1 | pass |  |
| `mmap03` | 0/1 | fail |  |
| `mmap04` | 0/1 | fail | TBROK: Expected 1 conversions got 0 FILE '/proc/self/maps' |
| `mmap05` | 0/1 | fail | TBROK: Test killed by SIGSEGV! |
| `mmap06` | 2/8 | partial | TFAIL: mmap(NULL, tc->length, tc->prot, tc->flags, fd, 0) succeeded |
| `mmap08` | 0/1 | fail | TFAIL: mmap(NULL, page_sz, PROT_WRITE, MAP_FILE / MAP_SHARED, fd, 0) expected EBADF: EINVAL (22) |
| `mmap09` | 3/3 | pass |  |
| `mmap12` | 0/1 | fail | TFAIL: pen dev pagemap failed: ENOENT (2) |
| `mmap13` | 0/1 | fail | TBROK: Test killed by SIGSEGV! |
| `mmap14` | 0/1 | fail | TFAIL: mmap14.c:100: Open dev status failed: errno=ENOENT(2): No such file or directory |
| `mmap15` | 1/1 | pass | EINVAL observed |
| `mmap16` | 0/1 | skip | TCONF: Couldn't find 'mkfs.ext4' in $PATH |
| `mmap17` | 1/1 | pass |  |
| `mmap18` | 0/8 | fail | TBROK: mmap(0x109000,4096,PROT_READ / PROT_WRITE(3),306,-1,0) failed: EINVAL (22) |
| `mmap19` | 1/1 | pass |  |
| `mmap20` | 0/1 | fail | TFAIL: mmap() failed with unexpected error: EINVAL (22) |
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
| `mremap03` | 0/1 | fail | TFAIL: mremap03.c:135: mremap() Fails, 'Unexpected errno 22 |
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
| `remap_file_pages01` | 0/15 | fail | TFAIL: remap_file_pages01.c:174: remap_file_pages error for page=0x402000, remap_sz=8192, window_pages=14: errno=ENOSYS(38): Function not implemented |
| `remap_file_pages02` | 0/1 | skip | TCONF: syscall(234) __NR_remap_file_pages not supported on your arch |
| `sbrk01` | 1/3 | partial | TFAIL: sbrk(8192) failed: ENOMEM (12) |
| `sbrk02` | 1/1 | pass |  |
| `sbrk03` | 0/1 | skip | TCONF: This arch 'unknown' is not supported for test! |
| `set_mempolicy01` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `set_mempolicy02` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `set_mempolicy03` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
| `set_mempolicy04` | 0/1 | skip | TCONF: test requires libnuma development packages with LIBNUMA_API_VERSION >= 2 |
