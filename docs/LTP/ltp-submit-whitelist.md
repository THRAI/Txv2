# LTP Submit Whitelist Progress

This file records the positive-score LTP cases included by the submit whitelist
in `crates/tx-kernel/src/init/exec.rs::LTP_SUBMIT_CASES`.

Source rule: collect cases with nonzero passed score from
`docs/LTP/syscalls/ltp-*-progress.md`, excluding the overlapping
`ltp-progress.md` p0 summary, then deduplicate by first occurrence.
Partial-score cases are included because they still add points.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| whitelist cases | 610 | deduplicated positive-score cases |
| stitched score | `4156/4987` | local documented score, not a single official full-run result |
| LA recorded cases | 610 | all whitelist cases have `LA Status` / `LA Note` recorded |
| LA stitched score | `4107/4956` | local LA documented score, not a single official full-run result |
| p0 source | excluded | p0 overlaps module batches |
| local command | `make oscomp-local-rv64-ltp-batch LTP_BATCH=submit` | mirrors the no-`tx.oscomp.groups` submit path |

## By Module

| Module | Cases | Score |
| --- | ---: | ---: |
| aio | 1 | `1/2` |
| cred | 39 | `125/155` |
| event | 43 | `439/493` |
| fd-io | 151 | `828/1015` |
| heavy | 11 | `18/142` |
| ipc | 43 | `245/324` |
| mount | 2 | `4/7` |
| process | 64 | `311/380` |
| sched | 17 | `73/79` |
| signal | 22 | `561/570` |
| smoke | 20 | `131/158` |
| time | 35 | `284/304` |
| vfs | 114 | `1023/1196` |
| vm | 48 | `113/162` |
| total | 610 | `4156/4987` |

## Cases

| Case | Module | Area | Status | Score | LA Status | LA Note |
| --- | --- | --- | --- | ---: | --- | --- |
| `io_uring01` | aio | aio | partial | `1/2` | partial | LA 1/2; mmap SQ/CQ ring returns EINVAL, exits cleanly |
| `capget01` | cred | cred | pass | `6/6` | pass | LA 6/6 |
| `capset01` | cred | cred | pass | `3/3` | pass | LA 3/3 |
| `capset04` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getegid02` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getegid02_16` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `geteuid01` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `geteuid02` | cred | cred | partial | `1/2` | partial | LA 1/2; /proc/self/status conversion count is 0, exits cleanly |
| `getgid01` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getgid03` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getresgid01` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getresgid02` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getresgid03` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getresuid01` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getresuid02` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getresuid03` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getuid01` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `getuid03` | cred | cred | partial | `1/2` | partial | LA 1/2; /proc/self/status conversion count is 0, exits cleanly |
| `setegid01` | cred | cred | pass | `4/4` | pass | LA 4/4 |
| `setgid01` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `setgid03` | cred | cred | pass | `2/2` | pass | LA 2/2 |
| `setgroups02` | cred | cred | partial | `1/3` | partial | LA 1/3; getgroups returns ENOSYS and group value remains 0 |
| `setgroups03` | cred | cred | partial | `1/3` | partial | LA 1/3; invalid setgroups cases unexpectedly succeed |
| `setregid01` | cred | cred | pass | `5/5` | pass | LA 5/5 |
| `setregid03` | cred | cred | partial | `16/22` | partial | LA 16/22; primary gid denial/saved gid checks mismatch |
| `setregid04` | cred | cred | pass | `9/9` | pass | LA 9/9 |
| `setresgid01` | cred | cred | pass | `5/5` | pass | LA 5/5 |
| `setresgid02` | cred | cred | pass | `6/6` | pass | LA 6/6 |
| `setresgid04` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `setresuid01` | cred | cred | pass | `9/9` | pass | LA 9/9 |
| `setresuid02` | cred | cred | pass | `4/4` | pass | LA 4/4 |
| `setresuid04` | cred | cred | partial | `1/3` | partial | LA 1/3; non-root open permission checks are too permissive |
| `setresuid05` | cred | cred | pass | `2/2` | pass | LA 2/2 |
| `setreuid01` | cred | cred | pass | `7/7` | pass | LA 7/7 |
| `setreuid02` | cred | cred | pass | `7/7` | pass | LA 7/7 |
| `setreuid03` | cred | cred | partial | `4/14` | partial | LA 4/14; non-root setreuid cases unexpectedly succeed |
| `setreuid04` | cred | cred | pass | `3/3` | pass | LA 3/3 |
| `setreuid05` | cred | cred | partial | `11/15` | partial | LA 11/15; saved uid and non-root setreuid checks mismatch |
| `setreuid07` | cred | cred | partial | `1/3` | partial | LA 1/3; non-root open permission checks are too permissive |
| `setuid01` | cred | cred | pass | `1/1` | pass | LA 1/1 |
| `epoll_create01` | event | event | partial | `2/3` | partial | LA 2/3; TCONF: syscall(-1) __NR_epoll_create not supported on your arch |
| `epoll_create1_01` | event | event | pass | `2/2` | pass | LA 2/2 |
| `epoll_create1_02` | event | event | pass | `2/2` | pass | LA 2/2 |
| `epoll_ctl01` | event | event | pass | `3/3` | pass | LA 3/3 |
| `epoll_ctl02` | event | event | pass | `9/9` | pass | LA 9/9 |
| `epoll_ctl03` | event | event | pass | `256/256` | pass | LA 256/256 |
| `epoll_ctl04` | event | event | pass | `1/1` | pass | LA 1/1 |
| `epoll_ctl05` | event | event | pass | `1/1` | pass | LA 1/1 |
| `epoll_wait01` | event | event | pass | `3/3` | pass | LA 3/3 |
| `epoll_wait02` | event | event | pass | `7/7` | pass | LA 7/7 |
| `epoll_wait03` | event | event | pass | `5/5` | pass | LA 5/5 |
| `epoll_wait06` | event | event | pass | `9/9` | pass | LA 9/9 |
| `epoll_wait07` | event | event | pass | `5/5` | pass | LA 5/5 |
| `eventfd01` | event | event | pass | `4/4` | pass | LA 4/4 |
| `eventfd02` | event | event | pass | `5/5` | pass | LA 5/5 |
| `eventfd03` | event | event | pass | `3/3` | pass | LA 3/3 |
| `eventfd04` | event | event | pass | `3/3` | pass | LA 3/3 |
| `eventfd05` | event | event | pass | `2/2` | pass | LA 2/2 |
| `eventfd2_01` | event | event | pass | `2/2` | pass | LA 2/2 |
| `eventfd2_02` | event | event | pass | `2/2` | pass | LA 2/2 |
| `eventfd2_03` | event | event | pass | `2/2` | pass | LA 2/2 |
| `futex_cmp_requeue02` | event | event | partial | `1/3` | partial | LA 1/3; TFAIL: futex_cmp_requeue() succeeded unexpectedly |
| `futex_wait01` | event | event | pass | `4/4` | pass | LA 4/4 |
| `futex_wait02` | event | event | pass | `1/1` | pass | LA 1/1 |
| `futex_wait03` | event | event | pass | `1/1` | fail | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `futex_wait04` | event | event | pass | `1/1` | pass | LA 1/1 |
| `futex_wait05` | event | event | pass | `7/7` | pass | LA 7/7 |
| `futex_wait_bitset01` | event | event | pass | `2/2` | pass | LA 2/2 |
| `futex_wake01` | event | event | pass | `6/6` | pass | LA 6/6 |
| `futex_wake03` | event | event | pass | `11/11` | pass | LA 11/11 |
| `poll01` | event | event | pass | `2/2` | pass | LA 2/2 |
| `poll02` | event | event | pass | `7/7` | pass | LA 7/7 |
| `ppoll01` | event | event | partial | `18/20` | partial | LA 18/20; TFAIL: ret: 0, exp: -1, ret_errno: SUCCESS (0), exp_errno: EINTR (4) |
| `pselect01` | event | event | partial | `1/7` | fail | LA 0/7; TFAIL: pselect() woken up early 468 times range: [999,882] |
| `pselect01_64` | event | event | partial | `1/7` | fail | LA 0/7; TFAIL: pselect() woken up early 424 times range: [999,895] |
| `pselect02` | event | event | pass | `3/3` | pass | LA 3/3 |
| `pselect02_64` | event | event | pass | `3/3` | pass | LA 3/3 |
| `pselect03` | event | event | pass | `1/1` | pass | LA 1/1 |
| `pselect03_64` | event | event | pass | `1/1` | pass | LA 1/1 |
| `select01` | event | event | partial | `6/13` | partial | LA 6/13; TFAIL: select() with regular file timed out |
| `select02` | event | event | partial | `14/17` | partial | LA 14/17; TCONF: syscall(-1) __NR_select not supported on your arch |
| `select03` | event | event | partial | `16/40` | partial | LA 16/40; TCONF: syscall(-1) __NR_select not supported on your arch |
| `select04` | event | event | partial | `4/7` | partial | LA 4/7; TCONF: syscall(-1) __NR_select not supported on your arch |
| `close01` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `close02` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `copy_file_range03` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `dup01` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `dup02` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `dup03` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `dup04` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `dup05` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `dup06` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `dup07` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `dup201` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `dup202` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `dup203` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `dup204` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `dup205` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `dup206` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `dup207` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `dup3_01` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `dup3_02` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `fallocate01` | fd-io | fd-io | pass | `2/2` | pass | LA 4/4 |
| `fallocate02` | fd-io | fd-io | pass | `8/8` | pass | LA 8/8 |
| `fallocate03` | fd-io | fd-io | pass | `8/8` | pass | LA 8/8 |
| `fcntl01` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl01_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl02` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `fcntl02_64` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `fcntl03` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl03_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl04` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl04_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl05` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `fcntl05_64` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `fcntl07` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `fcntl07_64` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `fcntl08` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl08_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl09` | fd-io | fd-io | pass | `2/2` | pass | LA 4/4 |
| `fcntl09_64` | fd-io | fd-io | pass | `2/2` | pass | LA 4/4 |
| `fcntl10` | fd-io | fd-io | pass | `2/2` | pass | LA 4/4 |
| `fcntl10_64` | fd-io | fd-io | pass | `2/2` | pass | LA 4/4 |
| `fcntl12` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl12_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl13` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `fcntl13_64` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `fcntl15_64` | fd-io | fd-io | partial | `2/3` | partial | LA 2/3; TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `fcntl15` | fd-io | fd-io | partial | `2/3` | partial | LA 2/3; TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `fcntl16` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl16_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl18` | fd-io | fd-io | pass | `1/1` | pass | LA 3/3 |
| `fcntl18_64` | fd-io | fd-io | pass | `1/1` | pass | LA 3/3 |
| `fcntl22` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl22_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fcntl27` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `fcntl27_64` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `fcntl29` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `fcntl29_64` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `fcntl30` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `fcntl30_64` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `fcntl34` | fd-io | fd-io | pass | `1/1` | fail | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `fcntl34_64` | fd-io | fd-io | pass | `1/1` | fail | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `fcntl36_64` | fd-io | fd-io | pass | `7/7` | fail | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `fcntl36` | fd-io | fd-io | pass | `7/7` | fail | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `fdatasync01` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fsync02` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `fsync03` | fd-io | fd-io | partial | `2/5` | partial | LA 2/5; TFAIL: fsync(): unexpected error: ENODEV (19) |
| `ioctl_ns07` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `llseek01` | fd-io | fd-io | partial | `1/2` | partial | LA 1/2; TFAIL: write successful after file size limit |
| `llseek02` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `llseek03` | fd-io | fd-io | pass | `18/18` | pass | LA 18/18 |
| `lseek01` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `lseek02` | fd-io | fd-io | partial | `9/15` | partial | LA 9/15; TFAIL: lseek(4, 1, 0) succeeded unexpectedly |
| `lseek07` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `pipe01` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pipe03` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `pipe04` | fd-io | fd-io | pass | `1/1` | pass | LA 2/2 |
| `pipe05` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pipe06` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pipe07` | fd-io | fd-io | partial | `1/2` | partial | LA 1/2; TFAIL: exp_num_pipes (1024) != num_pipe_fds (1020) |
| `pipe08` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pipe09` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pipe10` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pipe11` | fd-io | fd-io | pass | `70/70` | pass | LA 70/70 |
| `pipe12` | fd-io | fd-io | partial | `1/2` | partial | LA 1/2; TBROK: ioctl(4,(0x541B),...) failed: ENOTTY (25) |
| `pipe14` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pipe2_01` | fd-io | fd-io | pass | `7/7` | partial | LA 4/5; TBROK: pipe2({-1,-1}) failed with flag(16384): EINVAL (22) |
| `posix_fadvise01` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `posix_fadvise01_64` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `posix_fadvise02` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `posix_fadvise02_64` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `posix_fadvise03` | fd-io | fd-io | pass | `32/32` | pass | LA 32/32 |
| `posix_fadvise03_64` | fd-io | fd-io | pass | `32/32` | pass | LA 32/32 |
| `posix_fadvise04` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `posix_fadvise04_64` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `pread01` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pread01_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pread02` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `pread02_64` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `preadv01` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `preadv01_64` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `preadv02` | fd-io | fd-io | pass | `8/8` | pass | LA 8/8 |
| `preadv02_64` | fd-io | fd-io | pass | `8/8` | pass | LA 8/8 |
| `preadv201` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `preadv201_64` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `preadv202` | fd-io | fd-io | pass | `8/8` | pass | LA 8/8 |
| `preadv202_64` | fd-io | fd-io | pass | `8/8` | pass | LA 8/8 |
| `pwrite01` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pwrite01_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pwrite02` | fd-io | fd-io | pass | `5/5` | pass | LA 5/5 |
| `pwrite02_64` | fd-io | fd-io | pass | `5/5` | pass | LA 5/5 |
| `pwrite03` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pwrite03_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pwrite04` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pwrite04_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `pwritev01` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `pwritev01_64` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `pwritev02` | fd-io | fd-io | pass | `7/7` | pass | LA 7/7 |
| `pwritev02_64` | fd-io | fd-io | pass | `7/7` | pass | LA 7/7 |
| `pwritev201` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `pwritev201_64` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `pwritev202` | fd-io | fd-io | pass | `7/7` | pass | LA 7/7 |
| `pwritev202_64` | fd-io | fd-io | pass | `7/7` | pass | LA 7/7 |
| `read01` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `read02` | fd-io | fd-io | partial | `3/5` | partial | LA 3/5; TCONF: O_DIRECT not supported on tmpfs filesystem |
| `read04` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `readahead01` | fd-io | fd-io | partial | `15/25` | partial | LA 15/25; TCONF: pidfd_open(): ENOSYS (38) |
| `readv01` | fd-io | fd-io | pass | `10/10` | pass | LA 10/10 |
| `readv02` | fd-io | fd-io | partial | `4/5` | partial | LA 4/5; TFAIL: readv(3, 0x120038820, 1) succeeded |
| `sendfile02` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `sendfile02_64` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `sendfile03` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `sendfile03_64` | fd-io | fd-io | pass | `4/4` | pass | LA 4/4 |
| `sendfile04` | fd-io | fd-io | pass | `5/5` | pass | LA 5/5 |
| `sendfile04_64` | fd-io | fd-io | pass | `5/5` | pass | LA 5/5 |
| `sendfile05` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `sendfile05_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `sendfile06` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `sendfile06_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `sendfile08` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `sendfile08_64` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `splice07` | fd-io | fd-io | partial | `217/377` | partial | LA 217/377; TCONF: pidfd_open(): ENOSYS (38) |
| `sync_file_range01` | fd-io | fd-io | pass | `5/5` | pass | LA 5/5 |
| `write01` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `write02` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `write03` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `write05` | fd-io | fd-io | pass | `3/3` | pass | LA 3/3 |
| `write06` | fd-io | fd-io | pass | `2/2` | pass | LA 2/2 |
| `writev01` | fd-io | fd-io | pass | `6/6` | pass | LA 6/6 |
| `writev02` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `writev05` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `writev06` | fd-io | fd-io | pass | `1/1` | pass | LA 1/1 |
| `writev07` | fd-io | fd-io | pass | `8/8` | pass | LA 8/8 |
| `getdomainname01` | heavy | heavy | pass | `1/1` | pass | LA 1/1 |
| `modify_ldt01` | heavy | heavy | pass | `1/1` | pass | LA 1/1 |
| `modify_ldt02` | heavy | heavy | pass | `1/1` | pass | LA 1/1 |
| `modify_ldt03` | heavy | heavy | pass | `1/1` | pass | LA 1/1 |
| `newuname01` | heavy | heavy | pass | `1/1` | pass | LA 1/1 |
| `ptrace05` | heavy | heavy | partial | `1/124` | partial | LA 1/124; TFAIL: ptrace05.c:96: Failed to ptrace(PTRACE_TRACEME, ...) properly: errno=ENOSYS(38): Function not implemented |
| `sethostname01` | heavy | heavy | pass | `2/2` | pass | LA 2/2 |
| `sethostname02` | heavy | heavy | pass | `6/6` | pass | LA 6/6 |
| `uname01` | heavy | heavy | pass | `2/2` | pass | LA 2/2 |
| `uname02` | heavy | heavy | pass | `1/1` | pass | LA 1/1 |
| `uname04` | heavy | heavy | partial | `1/2` | partial | LA 1/2; TBROK: persona(131072) failed: ENOSYS (38) |
| `mq_notify01` | ipc | ipc | partial | `6/7` | partial | LA 2/3; TBROK: Test killed by SIGSEGV! |
| `mq_notify03` | ipc | ipc | partial | `1/2` | partial | LA 1/2; TBROK: Test killed by SIGSEGV! |
| `mq_open01` | ipc | ipc | partial | `5/10` | partial | LA 5/10; TBROK: Failed to open FILE '/proc/sys/fs/mqueue/queues_max' for reading: ENOENT (2) |
| `mq_timedreceive01` | ipc | ipc | partial | `24/30` | partial | LA 24/30; TFAIL: mq_timedreceive() failed unexpectedly, expected EINVAL: EAGAIN/EWOULDBLOCK (11) |
| `mq_timedsend01` | ipc | ipc | partial | `28/34` | partial | LA 28/34; TFAIL: mq_timedsend() failed unexpectedly, expected EINVAL: EAGAIN/EWOULDBLOCK (11) |
| `mq_unlink01` | ipc | ipc | partial | `3/4` | partial | LA 3/4; TFAIL: mq_unlink returned 0, expected -1, expected errno EACCES (13): SUCCESS (0) |
| `msgctl01` | ipc | ipc | partial | `13/14` | partial | LA 13/14; TFAIL: msg_ctime = 0, expected 1779494408 |
| `msgctl02` | ipc | ipc | partial | `1/2` | partial | LA 1/2; TFAIL: msg_qbytes = 16384, expected 16383 |
| `msgctl03` | ipc | ipc | pass | `2/2` | pass | LA 2/2 |
| `msgctl04` | ipc | ipc | partial | `12/14` | partial | LA 12/14; TCONF: EFAULT is skipped for libc variant |
| `msgctl06` | ipc | ipc | partial | `2/10` | partial | LA 2/10; TFAIL: MSG_INFO haven't returned a valid index: EINVAL (22) |
| `msgctl12` | ipc | ipc | partial | `3/4` | partial | LA 3/4; TFAIL: msgctl() test MSG_STAT failed with errno: 22 |
| `msgget01` | ipc | ipc | pass | `1/1` | pass | LA 1/1 |
| `msgget02` | ipc | ipc | pass | `6/6` | pass | LA 6/6 |
| `msgrcv01` | ipc | ipc | partial | `2/4` | partial | LA 2/4; TFAIL: PID of last msgrcv(2) mismatched |
| `msgrcv02` | ipc | ipc | partial | `4/8` | partial | LA 4/8; TFAIL: msgrcv(5, 0x1200391c0, -1, 2, 0) succeeded |
| `msgrcv07` | ipc | ipc | partial | `11/13` | partial | LA 11/13; TFAIL: MSG_EXCEPT didn't get MSGTYPE1 message |
| `msgrcv08` | ipc | ipc | pass | `1/1` | pass | LA 1/1 |
| `msgsnd01` | ipc | ipc | partial | `1/3` | partial | LA 1/3; TFAIL: PID of last msgsnd(2) mismatched |
| `semctl01` | ipc | ipc | partial | `8/12` | partial | LA 8/12; TBROK: semctl(0, 0, 18,...) failed: EINVAL (22) |
| `semctl02` | ipc | ipc | pass | `1/1` | pass | LA 1/1 |
| `semctl03` | ipc | ipc | partial | `6/8` | partial | LA 6/8; TCONF: EFAULT is skipped for libc variant |
| `semctl04` | ipc | ipc | pass | `2/2` | pass | LA 2/2 |
| `semctl05` | ipc | ipc | pass | `3/3` | pass | LA 3/3 |
| `semctl06` | ipc | ipc | pass | `1/1` | pass | LA 1/1 |
| `semctl07` | ipc | ipc | pass | `16/16` | pass | LA 16/16 |
| `semctl09` | ipc | ipc | partial | `4/16` | partial | LA 4/16; TFAIL: SEM_INFO haven't returned a valid index: EINVAL (22) |
| `semget01` | ipc | ipc | pass | `3/3` | pass | LA 3/3 |
| `semget02` | ipc | ipc | pass | `6/6` | pass | LA 6/6 |
| `semop01` | ipc | ipc | pass | `4/4` | pass | LA 4/4 |
| `semop02` | ipc | ipc | partial | `19/26` | partial | LA 19/26; TFAIL: semop failed unexpectedly; expected: E2BIG: EINVAL (22) |
| `semop03` | ipc | ipc | pass | `8/8` | pass | LA 8/8 |
| `semop04` | ipc | ipc | pass | `1/1` | pass | LA 1/1 |
| `semop05` | ipc | ipc | pass | `1/1` | fail | LA 0/1 |
| `shmat01` | ipc | ipc | pass | `4/4` | pass | LA 4/4 |
| `shmat02` | ipc | ipc | pass | `3/3` | pass | LA 3/3 |
| `shmat04` | ipc | ipc | pass | `1/1` | pass | LA 1/1 |
| `shmctl02` | ipc | ipc | partial | `16/22` | partial | LA 16/22; TFAIL: shmctl(4, 11, 0x120038c38) expected EPERM: EINVAL (22) |
| `shmctl07` | ipc | ipc | partial | `1/4` | partial | LA 1/4; TFAIL: shmctl(12, SHM_LOCK, NULL): EINVAL (22) |
| `shmctl08` | ipc | ipc | partial | `5/6` | partial | LA 5/6; TFAIL: shm_ctime not updated old 0 new 0 |
| `shmdt01` | ipc | ipc | partial | `1/2` | partial | LA 1/2; TBROK: Test killed by SIGSEGV! |
| `shmdt02` | ipc | ipc | pass | `2/2` | pass | LA 2/2 |
| `shmget04` | ipc | ipc | pass | `3/3` | pass | LA 3/3 |
| `setns01` | mount | mount | partial | `3/5` | partial | LA 3/5; TFAIL: without CAP_SYS_ADMIN ret=0 expected=-1 |
| `unshare02` | mount | mount | partial | `1/2` | partial | LA 1/2; TFAIL: unshare(CLONE_NEWNS) expected EPERM: EINVAL (22) |
| `clone01` | process | process | pass | `2/2` | pass | LA 2/2 |
| `clone02` | process | process | pass | `2/2` | pass | LA 2/2 |
| `clone03` | process | process | pass | `1/1` | pass | LA 1/1 |
| `clone05` | process | process | pass | `1/1` | pass | LA 1/1 |
| `clone06` | process | process | pass | `1/1` | pass | LA 1/1 |
| `clone07` | process | process | pass | `1/1` | pass | LA 1/1 |
| `clone08` | process | process | partial | `3/5` | partial | LA 1/4; TBROK: CLONE_PARENT clone() failed: EINVAL (22) |
| `clone302` | process | process | partial | `1/2` | partial | LA 1/2; TCONF: syscall(435) __NR_clone3 not supported on your arch |
| `execl01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `execle01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `execlp01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `execv01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `execve01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `execve03` | process | process | partial | `3/6` | partial | LA 3/6; TFAIL: execve failed unexpectedly; expected Filename too long: ENOENT (2) |
| `execve06` | process | process | pass | `1/1` | pass | LA 1/1 |
| `execvp01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `exit01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `exit02` | process | process | pass | `1/1` | pass | LA 1/1 |
| `exit_group01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `fork01` | process | process | pass | `2/2` | pass | LA 2/2 |
| `fork03` | process | process | pass | `1/1` | pass | LA 1/1 |
| `fork04` | process | process | partial | `1/2` | partial | LA 1/2; TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `fork07` | process | process | pass | `1/1` | pass | LA 1/1 |
| `fork08` | process | process | pass | `1/1` | pass | LA 1/1 |
| `fork09` | process | process | pass | `1/1` | pass | LA 1/1 |
| `fork10` | process | process | pass | `2/2` | pass | LA 2/2 |
| `get_robust_list01` | process | process | partial | `4/5` | partial | LA 4/5; TFAIL: get_robust_list01.c:172: get_robust_list failed unexpectedly: errno=ESRCH(3): No such process |
| `getpgid01` | process | process | partial | `4/8` | partial | LA 4/8; TFAIL: getpgid(16) failed: ESRCH (3) |
| `getpgid02` | process | process | pass | `2/2` | pass | LA 2/2 |
| `getpgrp01` | process | process | pass | `2/2` | pass | LA 2/2 |
| `getpid01` | process | process | pass | `100/100` | pass | LA 100/100 |
| `getpid02` | process | process | pass | `2/2` | pass | LA 2/2 |
| `getppid01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `getppid02` | process | process | pass | `1/1` | pass | LA 1/1 |
| `getsid01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `getsid02` | process | process | pass | `1/1` | pass | LA 1/1 |
| `gettid01` | process | process | pass | `2/2` | pass | LA 2/2 |
| `gettid02` | process | process | pass | `11/11` | fail | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `kcmp01` | process | process | pass | `5/5` | pass | LA 5/5 |
| `kcmp02` | process | process | pass | `6/6` | pass | LA 6/6 |
| `personality01` | process | process | pass | `18/18` | pass | LA 18/18 |
| `personality02` | process | process | pass | `1/1` | pass | LA 1/1 |
| `pidfd_getfd01` | process | process | partial | `1/3` | partial | LA 1/3; fd duplication passes, checkpoint cleanup times out |
| `pidfd_getfd02` | process | process | partial | `3/5` | partial | LA 3/5; first errno cases pass, checkpoint paths time out |
| `pidfd_open01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `pidfd_open02` | process | process | pass | `3/3` | pass | LA 3/3 |
| `pidfd_open04` | process | process | partial | `1/4` | partial | LA 1/4; `PIDFD_NONBLOCK` passes, `waitid(P_PIDFD)` still returns ENOSYS and checkpoint cleanup times out |
| `pidfd_send_signal02` | process | process | pass | `4/4` | pass | LA 4/4 |
| `set_robust_list01` | process | process | partial | `1/2` | partial | LA 1/2; TFAIL: set_robust_list01.c:117: set_robust_list: retval = 0 (expected -1), errno = 0 (expected 22) |
| `set_tid_address01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `setpgid01` | process | process | partial | `1/2` | partial | LA 1/2; TFAIL: setpgid01.c:87: test setpgid(9, 1) fail: TEST_ERRNO=ENOSYS(38): Function not implemented |
| `setpgrp01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `setpgrp02` | process | process | pass | `2/2` | pass | LA 2/2 |
| `setsid01` | process | process | partial | `2/4` | partial | LA 2/4; TFAIL: setsid01.c:155: setpgid failed, errno :38 |
| `vfork01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `wait01` | process | process | pass | `1/1` | pass | LA 1/1 |
| `wait02` | process | process | pass | `1/1` | pass | LA 1/1 |
| `wait402` | process | process | pass | `1/1` | pass | LA 1/1 |
| `waitid04` | process | process | partial | `1/4` | partial | LA 1/4; checkpoint wait/wake time out, exits cleanly |
| `waitid05` | process | process | partial | `1/6` | partial | LA 1/6; TFAIL: waitid(P_PGID, pid_group+1, infop, WEXITED) expected ECHILD: ENOSYS (38) |
| `waitid06` | process | process | partial | `1/6` | partial | LA 1/6; TFAIL: waitid(P_PID, pid_child+1, infop, WEXITED) expected ECHILD: ENOSYS (38) |
| `waitpid01` | process | process | partial | `84/115` | partial | LA 84/115; TFAIL: WIFSIGNALED() not set in status (exited with 0) |
| `waitpid03` | process | process | pass | `2/2` | pass | LA 2/2 |
| `waitpid04` | process | process | partial | `2/4` | partial | LA 2/4; TFAIL: waipid(-1, NULL, 0xffffffff) expected EINVAL: ECHILD (10) |
| `getrlimit01` | sched | sched | pass | `16/16` | pass | LA 16/16 |
| `getrlimit02` | sched | sched | pass | `2/2` | pass | LA 2/2 |
| `getrlimit03` | sched | sched | pass | `16/16` | pass | LA 16/16 |
| `getrusage01` | sched | sched | pass | `2/2` | pass | LA 2/2 |
| `getrusage02` | sched | sched | partial | `3/4` | partial | LA 3/4; TCONF: EFAULT is skipped for libc variant |
| `membarrier01` | sched | sched | partial | `3/4` | partial | LA 3/4; TBROK: Test 3 haven't reported results! |
| `sched_getaffinity01` | sched | sched | pass | `4/4` | pass | LA 4/4 |
| `sched_getattr01` | sched | sched | pass | `1/1` | pass | LA 1/1 |
| `sched_getattr02` | sched | sched | pass | `4/4` | pass | LA 4/4 |
| `sched_setaffinity01` | sched | sched | pass | `4/4` | pass | LA 4/4 |
| `sched_setattr01` | sched | sched | pass | `4/4` | fail | LA 0/4 |
| `sched_setscheduler01` | sched | sched | pass | `8/8` | partial | LA 4/5; TCONF: sched_setscheduler not supported |
| `setrlimit01` | sched | sched | partial | `2/4` | partial | LA 2/4; TFAIL: setrlimit01.c:183: setrlimit failed, expected 10 got 26 |
| `setrlimit02` | sched | sched | partial | `1/2` | partial | LA 1/2; TFAIL: call succeeded unexpectedly |
| `setrlimit03` | sched | sched | partial | `1/2` | partial | LA 1/2; TFAIL: call succeeded unexpectedly (nr_open=1048576 rlim_cur=1024 rlim_max=1048577) |
| `setrlimit04` | sched | sched | pass | `1/1` | pass | LA 1/1 |
| `setrlimit05` | sched | sched | pass | `1/1` | pass | LA 1/1 |
| `kill02` | signal | signal | pass | `2/2` | pass | LA 2/2 |
| `kill06` | signal | signal | pass | `1/1` | pass | LA 1/1 |
| `kill07` | signal | signal | pass | `1/1` | pass | LA 1/1 |
| `kill08` | signal | signal | pass | `1/1` | pass | LA 1/1 |
| `kill09` | signal | signal | pass | `1/1` | pass | LA 1/1 |
| `kill12` | signal | signal | pass | `1/1` | pass | LA 1/1 |
| `rt_sigaction01` | signal | signal | pass | `150/150` | pass | LA 150/150 |
| `rt_sigaction02` | signal | signal | pass | `150/150` | pass | LA 150/150 |
| `rt_sigaction03` | signal | signal | pass | `150/150` | pass | LA 150/150 |
| `rt_sigprocmask02` | signal | signal | pass | `2/2` | pass | LA 2/2 |
| `sigaction01` | signal | signal | partial | `1/2` | partial | LA 1/2; TFAIL: sigaction01.c:125: SA_RESETHAND should not cause SA_SIGINFO to be cleared, but it was. |
| `sigaction02` | signal | signal | partial | `1/5` | pass | LA 3/3 |
| `sigaltstack01` | signal | signal | pass | `1/1` | pass | LA 1/1 |
| `sigaltstack02` | signal | signal | pass | `2/2` | pass | LA 2/2 |
| `signal02` | signal | signal | partial | `1/3` | pass | LA 3/3 |
| `signal03` | signal | signal | pass | `31/31` | pass | LA 31/31 |
| `signal04` | signal | signal | pass | `28/28` | pass | LA 28/28 |
| `signal05` | signal | signal | partial | `30/31` | partial | LA 30/31; TFAIL: siglist[n] (18) != sig_pass (17) |
| `signalfd01` | signal | signal | pass | `2/2` | pass | LA 2/2 |
| `signalfd4_01` | signal | signal | pass | `1/1` | pass | LA 1/1 |
| `signalfd4_02` | signal | signal | pass | `1/1` | pass | LA 1/1 |
| `sigwait01` | signal | signal | partial | `3/4` | partial | LA 3/4; TBROK: kill(15,SIGTERM) failed: ESRCH (3) |
| `confstr01` | smoke | libc | pass | `32/32` | pass | LA 36/36 |
| `fpathconf01` | smoke | libc | pass | `9/9` | pass | LA 9/9 |
| `gethostname01` | smoke | libc | pass | `1/1` | pass | LA 1/1 |
| `getpagesize01` | smoke | libc | pass | `1/1` | pass | LA 1/1 |
| `getrandom01` | smoke | random | pass | `4/4` | pass | LA 4/4 |
| `getrandom02` | smoke | random | pass | `4/4` | pass | LA 4/4 |
| `getrandom03` | smoke | random | pass | `9/9` | pass | LA 9/9 |
| `getrandom04` | smoke | random | pass | `1/1` | pass | LA 1/1 |
| `getrandom05` | smoke | random | pass | `2/2` | pass | LA 2/2 |
| `memcmp01` | smoke | string | pass | `2/2` | pass | LA 2/2 |
| `memcpy01` | smoke | string | pass | `2/2` | pass | LA 2/2 |
| `memset01` | smoke | string | pass | `1/1` | pass | LA 1/1 |
| `nftw01` | smoke | fs | partial | `1/2` | partial | LA 1/2; TFAIL: tools.c:267: Test failed |
| `nftw6401` | smoke | fs | partial | `1/2` | partial | LA 1/2; TFAIL: tools64.c:267: Test failed |
| `pathconf01` | smoke | fs | pass | `17/17` | pass | LA 17/17 |
| `pathconf02` | smoke | fs | partial | `1/6` | partial | LA 1/6; TFAIL: pathconf() fail with path prefix is not a directory invalid retval 8: SUCCESS (0) |
| `string01` | smoke | string | pass | `1/1` | pass | LA 1/1 |
| `syscall01` | smoke | syscall | pass | `3/3` | pass | LA 3/3 |
| `sysconf01` | smoke | libc | partial | `36/56` | partial | LA 36/56; TCONF: sysconf01.c:65: Not supported sysconf resource: _SC_CHILD_MAX |
| `ulimit01` | smoke | libc | pass | `3/3` | pass | LA 3/3 |
| `alarm02` | time | time | pass | `6/6` | pass | LA 6/6 |
| `alarm03` | time | time | pass | `2/2` | pass | LA 2/2 |
| `alarm05` | time | time | pass | `3/3` | pass | LA 3/3 |
| `alarm06` | time | time | pass | `2/2` | pass | LA 2/2 |
| `alarm07` | time | time | pass | `2/2` | pass | LA 2/2 |
| `clock_getres01` | time | time | pass | `44/44` | pass | LA 44/44 |
| `clock_gettime02` | time | time | pass | `10/10` | pass | LA 10/10 |
| `clock_nanosleep01` | time | time | partial | `11/14` | partial | LA 11/14; TFAIL: returned 0, expected -1, expected errno: EFAULT (14): SUCCESS (0) |
| `clock_nanosleep02` | time | time | pass | `7/7` | pass | LA 7/7 |
| `clock_nanosleep04` | time | time | pass | `4/4` | pass | LA 4/4 |
| `getitimer01` | time | time | pass | `30/30` | pass | LA 30/30 |
| `getitimer02` | time | time | pass | `3/3` | pass | LA 3/3 |
| `gettimeofday01` | time | time | partial | `2/3` | partial | LA 2/3; TFAIL: tst_syscall(__NR_gettimeofday, tc->tv, tc->tz) succeeded |
| `gettimeofday02` | time | time | pass | `1/1` | pass | LA 1/1 |
| `nanosleep01` | time | time | pass | `7/7` | pass | LA 7/7 |
| `nanosleep02` | time | time | pass | `2/2` | pass | LA 2/2 |
| `nanosleep04` | time | time | pass | `3/3` | pass | LA 3/3 |
| `setitimer01` | time | time | pass | `18/18` | pass | LA 18/18 |
| `setitimer02` | time | time | pass | `3/3` | pass | LA 3/3 |
| `settimeofday02` | time | time | partial | `1/3` | partial | LA 1/3; TFAIL: settimeofday(&tc->tv, NULL) expected EINVAL: ENOSYS (38) |
| `time01` | time | time | pass | `2/2` | pass | LA 2/2 |
| `timer_delete01` | time | time | pass | `8/8` | pass | LA 8/8 |
| `timer_delete02` | time | time | pass | `1/1` | pass | LA 1/1 |
| `timer_getoverrun01` | time | time | pass | `2/2` | pass | LA 2/2 |
| `timer_gettime01` | time | time | pass | `3/3` | pass | LA 3/3 |
| `timer_settime01` | time | time | pass | `32/32` | pass | LA 32/32 |
| `timer_settime02` | time | time | pass | `48/48` | pass | LA 48/48 |
| `timer_settime03` | time | time | pass | `1/1` | pass | LA 1/1 |
| `timerfd01` | time | time | partial | `3/12` | partial | LA 4/12; TFAIL: no ticks happened |
| `timerfd02` | time | time | pass | `6/6` | pass | LA 6/6 |
| `timerfd_create01` | time | time | pass | `2/2` | pass | LA 2/2 |
| `timerfd_gettime01` | time | time | pass | `3/3` | pass | LA 3/3 |
| `timerfd_settime01` | time | time | pass | `4/4` | pass | LA 4/4 |
| `times01` | time | time | pass | `1/1` | pass | LA 1/1 |
| `times03` | time | time | partial | `7/12` | partial | LA 7/12; TFAIL: buf1.tms_utime = 1157 |
| `access01` | vfs | vfs | partial | `147/199` | partial | LA 147/199; TFAIL: access(accessfile_r, W_OK) as nobody succeeded |
| `access02` | vfs | vfs | partial | `12/16` | partial | LA 12/16; TFAIL: execute file_x as root failed: SUCCESS (0) |
| `chdir04` | vfs | vfs | partial | `1/3` | partial | LA 1/3; TFAIL: chdir() expected ENAMETOOLONG: ENOENT (2) |
| `chmod01` | vfs | vfs | partial | `24/32` | partial | LA 16/32; TFAIL: stat(testfile) mode=0644 |
| `chmod03` | vfs | vfs | partial | `3/4` | partial | LA 2/4; TFAIL: stat(testfile) mode=100644 |
| `chmod05` | vfs | vfs | pass | `1/1` | fail | LA 0/1; TFAIL: testdir: Incorrect modes 040755, Expected 041777 |
| `chmod07` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `chown01` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `chown02` | vfs | vfs | partial | `2/3` | partial | LA 2/3; TFAIL: testfile1: wrong mode permissions 0106770, expected 0100770 |
| `chown03` | vfs | vfs | partial | `1/2` | partial | LA 1/2; TFAIL: chown03_testfile: wrong mode permissions 0106770, expected 0100770 |
| `chown05` | vfs | vfs | partial | `6/12` | partial | LA 6/12; TFAIL: testfile: incorrect ownership set, expected 700 701 |
| `creat01` | vfs | vfs | pass | `6/6` | pass | LA 6/6 |
| `creat03` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `creat05` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `creat08` | vfs | vfs | partial | `6/9` | fail | LA 0/1; TBROK: dir_a: Incorrect group, 0 != 1 |
| `faccessat01` | vfs | vfs | pass | `3/3` | pass | LA 3/3 |
| `faccessat02` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `faccessat201` | vfs | vfs | partial | `5/7` | partial | LA 5/7; TFAIL: faccessat2(-1, /tmp/LTP_facCCIfDj/faccessat2dir/faccessat2file, R_OK, 0) failed: EBADF (9) |
| `faccessat202` | vfs | vfs | partial | `2/6` | partial | LA 2/6; TFAIL: faccessat2() with invalid address expected EFAULT: ENAMETOOLONG (36) |
| `fchdir01` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `fchdir02` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `fchmod01` | vfs | vfs | pass | `8/8` | pass | LA 8/8 |
| `fchmod02` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `fchmod03` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `fchmod04` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `fchmod05` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `fchmodat01` | vfs | vfs | pass | `6/6` | pass | LA 6/6 |
| `fchmodat02` | vfs | vfs | partial | `5/6` | partial | LA 5/6; TFAIL: fchmodat() with invalid address expected EFAULT: ENAMETOOLONG (36) |
| `fchownat01` | vfs | vfs | pass | `5/5` | pass | LA 5/5 |
| `flock01` | vfs | vfs | pass | `3/3` | pass | LA 3/3 |
| `flock02` | vfs | vfs | pass | `3/3` | pass | LA 3/3 |
| `flock03` | vfs | vfs | partial | `1/3` | partial | LA 1/3; checkpoint wait/wake time out, exits cleanly |
| `flock04` | vfs | vfs | pass | `6/6` | pass | LA 6/6 |
| `flock06` | vfs | vfs | pass | `4/4` | pass | LA 4/4 |
| `fstat02` | vfs | vfs | pass | `6/6` | pass | LA 6/6 |
| `fstat02_64` | vfs | vfs | pass | `6/6` | pass | LA 6/6 |
| `fstat03` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `fstat03_64` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `fstatat01` | vfs | vfs | pass | `6/6` | pass | LA 6/6 |
| `fstatfs02` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `fstatfs02_64` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `ftruncate01` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `ftruncate01_64` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `ftruncate03` | vfs | vfs | partial | `3/4` | partial | LA 3/4; TFAIL: ftruncate() succeeded unexpectedly and got 0 |
| `ftruncate03_64` | vfs | vfs | partial | `3/4` | partial | LA 3/4; TFAIL: ftruncate() succeeded unexpectedly and got 0 |
| `getcwd01` | vfs | vfs | partial | `3/5` | partial | LA 3/5; TFAIL: tst_syscall(__NR_getcwd, tc->buf, tc->size) expected ERANGE: EINVAL (22) |
| `getcwd03` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `getdents02` | vfs | vfs | partial | `12/13` | partial | LA 8/10; TCONF: syscall(-1) __NR_getdents not supported on your arch |
| `lchown01` | vfs | vfs | pass | `6/6` | pass | LA 6/6 |
| `lchown02` | vfs | vfs | partial | `3/6` | partial | LA 3/6; TFAIL: lchown02.c:154: lchown(2) returned 0, expected -1, errno:1 |
| `link02` | vfs | vfs | partial | `1/2` | partial | LA 1/2; TFAIL: link(oldpath,newpath) returned 0 but stat link counts do not match 1 1 |
| `link04` | vfs | vfs | partial | `10/14` | partial | LA 10/14; TFAIL: link(<invalid address>, <nefile>) Failed expected errno: 14: ENAMETOOLONG (36) |
| `linkat01` | vfs | vfs | pass | `22/22` | pass | LA 22/22 |
| `lstat01A` | vfs | vfs | partial | `1/3` | partial | LA 1/3; symlink01 alias lstat symbolic-link cases TBROK |
| `lstat01A_64` | vfs | vfs | partial | `1/3` | partial | LA 1/3; symlink01 alias lstat symbolic-link cases TBROK |
| `lstat02` | vfs | vfs | partial | `4/6` | partial | LA 5/6; TFAIL: lstat() returned 0, expected -1: SUCCESS (0) |
| `lstat02_64` | vfs | vfs | partial | `4/6` | partial | LA 5/6; TFAIL: lstat() returned 0, expected -1: SUCCESS (0) |
| `mkdir05` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `mkdirat01` | vfs | vfs | pass | `5/5` | pass | LA 5/5 |
| `mknod01` | vfs | vfs | pass | `7/7` | pass | LA 7/7 |
| `mknod02` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `mknod05` | vfs | vfs | pass | `1/1` | fail | LA 0/2; setgid bit not reflected in created directory mode |
| `mknod06` | vfs | vfs | partial | `5/6` | partial | LA 5/6; TFAIL: mknod06.c:161: mknod() fails, Invalid address, errno:36, expected errno:14 |
| `mknod08` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `mknod09` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `mknodat01` | vfs | vfs | pass | `5/5` | pass | LA 5/5 |
| `name_to_handle_at01` | vfs | vfs | pass | `27/27` | pass | LA 27/27 |
| `name_to_handle_at02` | vfs | vfs | pass | `9/9` | pass | LA 9/9 |
| `open01` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `open02` | vfs | vfs | partial | `1/2` | partial | LA 1/2; TFAIL: open() unprivileged O_RDONLY / O_NOATIME succeeded |
| `open03` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `open04` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `open07` | vfs | vfs | partial | `1/5` | partial | LA 1/5; TFAIL: open(O_NOFOLLOW) a symlink to file succeeded |
| `open08` | vfs | vfs | partial | `2/6` | partial | LA 2/6; TFAIL: O_RDWR succeeded |
| `open09` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `open10` | vfs | vfs | partial | `6/9` | fail | LA 0/1; TBROK: dir_a: Incorrect group, 0 != 1 |
| `open11` | vfs | vfs | partial | `23/28` | partial | LA 23/28; TFAIL: open directory O_RDWR succeeded |
| `open12` | vfs | vfs | partial | `3/5` | partial | LA 3/5; TBROK: open12.c:224: write(3,0x1200233a8,11) failed: errno=EINVAL(22): Invalid argument |
| `open13` | vfs | vfs | partial | `2/5` | partial | LA 2/5; TFAIL: open13.c:144: fchmod(2) succeeded unexpectedly |
| `open_by_handle_at01` | vfs | vfs | pass | `9/9` | pass | LA 9/9 |
| `open_by_handle_at02` | vfs | vfs | pass | `7/7` | pass | LA 7/7 |
| `openat02` | vfs | vfs | partial | `2/4` | partial | LA 2/4; TBROK: openat02.c:199: write(3,0x120023588,7) failed: errno=EINVAL(22): Invalid argument |
| `prot_hsymlinks` | vfs | vfs | partial | `396/397` | partial | LA 396/397; TWARN: tst_tmpdir.c:342: tst_rmdir: rmobj(/tmp/LTP_proNAhOKM) failed: remove(/tmp/LTP_proNAhOKM) failed; errno=39: ENOTEMPTY |
| `readdir01` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `readlink01` | vfs | vfs | pass | `2/2` | partial | LA 2/4 |
| `readlink01A` | vfs | vfs | partial | `2/4` | partial | LA 2/4; TBROK: symlink01.c:986: lstat(2) Failure when accessing symbolic symbolic link file which should contain object path to (null) file |
| `readlink03` | vfs | vfs | partial | `7/8` | partial | LA 6/8; TFAIL: readlink() sueeeeded unexpectedly |
| `readlinkat01` | vfs | vfs | partial | `10/12` | partial | LA 10/12; TFAIL: readlinkat(5, , , 1024) failed: EINVAL (22) |
| `readlinkat02` | vfs | vfs | pass | `6/6` | partial | LA 5/6; TFAIL: readlinkat(3, symlink_file, NULL, 0) succeeded |
| `rmdir01` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `stat01` | vfs | vfs | pass | `12/12` | pass | LA 12/12 |
| `stat01_64` | vfs | vfs | pass | `12/12` | pass | LA 12/12 |
| `stat02` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `stat02_64` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `stat03` | vfs | vfs | partial | `4/6` | partial | LA 4/6; TFAIL: stat(tc->pathname, &stat_buf) succeeded |
| `stat03_64` | vfs | vfs | partial | `4/6` | partial | LA 4/6; TFAIL: stat(tc->pathname, &stat_buf) succeeded |
| `statfs02` | vfs | vfs | partial | `1/6` | partial | LA 1/6; TFAIL: statfs() succeeded |
| `statfs02_64` | vfs | vfs | partial | `1/6` | partial | LA 1/6; TFAIL: statfs() succeeded |
| `statx02` | vfs | vfs | partial | `4/5` | partial | LA 4/5; TFAIL: Statx symlink flag failed to work as expected |
| `statx03` | vfs | vfs | partial | `5/7` | partial | LA 5/7; TFAIL: statx() should fail with EFAULT: ENAMETOOLONG (36) |
| `symlink01` | vfs | vfs | partial | `1/5` | partial | LA 1/5; TBROK: symlink01.c:983: lstat(2) Failure when accessing symbolic symbolic link file which should contain %bc+eFhi!k path to (null) file |
| `symlink02` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `symlink03` | vfs | vfs | partial | `4/6` | partial | LA 4/6; TFAIL: symlink03.c:189: symlink() returned 0, expected -1, errno:13 |
| `symlink04` | vfs | vfs | partial | `2/4` | partial | LA 2/4; TBROK: lstat(slink_file,0x401ccb40) failed: ENOENT (2) |
| `symlinkat01` | vfs | vfs | pass | `10/10` | pass | LA 10/10 |
| `truncate02` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `truncate02_64` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `truncate03` | vfs | vfs | partial | `5/8` | partial | LA 5/8; TFAIL: truncate(tc->pathname, tc->length) succeeded |
| `truncate03_64` | vfs | vfs | partial | `5/8` | partial | LA 5/8; TFAIL: truncate(tc->pathname, tc->length) succeeded |
| `umask01` | vfs | vfs | pass | `1/1` | pass | LA 1/1 |
| `unlink05` | vfs | vfs | pass | `2/2` | pass | LA 2/2 |
| `unlink07` | vfs | vfs | partial | `5/6` | partial | LA 5/6; TFAIL: invalid address expected EFAULT: ENAMETOOLONG (36) |
| `unlink08` | vfs | vfs | partial | `2/4` | partial | LA 2/4; TFAIL: unwritable directory succeeded |
| `unlinkat01` | vfs | vfs | pass | `7/7` | pass | LA 7/7 |
| `brk01` | vm | vm | partial | `1/2` | partial | LA 1/2; TCONF: brk() not implemented |
| `brk02` | vm | vm | partial | `1/2` | partial | LA 1/2; TCONF: brk() not implemented |
| `madvise01` | vm | vm | partial | `6/20` | partial | LA 6/20; TFAIL: madvise test for MADV_REMOVE failed with return = -1, errno = 38 : ??? |
| `madvise02` | vm | vm | partial | `1/13` | partial | LA 1/13; TFAIL: MADV_NORMAL failed unexpectedly; expected - 22 : ???: ENOSYS (38) |
| `madvise05` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `madvise10` | vm | vm | partial | `2/6` | partial | LA 2/6; TFAIL: madvise(0x2000, 16384, 0x12): ENOSYS (38) |
| `mincore01` | vm | vm | pass | `4/4` | pass | LA 4/4 |
| `mincore02` | vm | vm | partial | `1/2` | partial | LA 1/2; TFAIL: locked_pages (0) != NUM_PAGES (4) |
| `mincore03` | vm | vm | partial | `1/2` | partial | LA 1/2; TFAIL: mincore reports resident pages as 0, but expected 3 |
| `mlock01` | vm | vm | pass | `4/4` | pass | LA 4/4 |
| `mlock02` | vm | vm | pass | `3/3` | pass | LA 3/3 |
| `mlock03` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mlock04` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mlock05` | vm | vm | pass | `2/2` | pass | LA 2/2 |
| `mlock201` | vm | vm | partial | `4/8` | partial | LA 4/8; TFAIL: mlock2(0) locked 0 pages, expected 1 |
| `mlock202` | vm | vm | pass | `4/4` | pass | LA 4/4 |
| `mlock203` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mlockall01` | vm | vm | pass | `3/3` | pass | LA 3/3 |
| `mlockall02` | vm | vm | partial | `1/3` | pass | LA 3/3 |
| `mlockall03` | vm | vm | pass | `3/3` | pass | LA 3/3 |
| `mmap01` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mmap02` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mmap04` | vm | vm | pass | `14/14` | pass | LA 14/14 |
| `mmap06` | vm | vm | pass | `8/8` | pass | LA 8/8 |
| `mmap08` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mmap09` | vm | vm | pass | `3/3` | pass | LA 3/3 |
| `mmap15` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mmap17` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mmap19` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mmap20` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mprotect01` | vm | vm | partial | `1/4` | partial | LA 1/4; TBROK: mprotect01.c:150: mmap failed |
| `mprotect03` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mprotect05` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mremap02` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mremap03` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mremap04` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `mremap05` | vm | vm | pass | `7/7` | pass | LA 7/7 |
| `mremap06` | vm | vm | pass | `3/3` | pass | LA 3/3 |
| `msync01` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `msync02` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `msync03` | vm | vm | partial | `2/6` | partial | LA 2/6; TFAIL: msync03.c:141: msync succeeded unexpectedly |
| `munlock01` | vm | vm | pass | `4/4` | pass | LA 4/4 |
| `munlock02` | vm | vm | pass | `1/1` | pass | LA 1/1 |
| `munlockall01` | vm | vm | pass | `2/2` | pass | LA 2/2 |
| `munmap03` | vm | vm | pass | `3/3` | pass | LA 3/3 |
| `remap_file_pages02` | vm | vm | pass | `4/4` | pass | LA 4/4 |
| `sbrk01` | vm | vm | partial | `1/3` | partial | LA 1/3; TFAIL: sbrk(8192) failed: ENOMEM (12) |
| `sbrk02` | vm | vm | pass | `1/1` | pass | LA 1/1 |
