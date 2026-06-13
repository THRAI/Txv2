# LTP event Progress

`event` batch local tracking. Cases are from `tools/ltp-batches.py --batch event`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 90 | from `make ltp-batch-cases LTP_BATCH=event` |
| latest local run | focused LA64 submit-tail rerun | 2026-06-03 promoted whitelist candidates, musl+glibc |
| cumulative scored | `441/562` | recorded rows in this document |
| reached case | `userfaultfd01` | batch completed |
| logs | `target/oscomp/ltp-progress/event`, `target/oscomp/ltp-timeout-triage/event` | per-group stdout, single-case timeout triage logs, and serial snapshots |

## 2026-06-03 focused submit-tail rerun

复测日志：

- LA musl: `target/oscomp/ltp-extra-core-a-la-musl-20260603.txt`
- LA glibc: `target/oscomp/ltp-extra-core-g1-la-glibc-20260603.txt`

确认可作为 active submit 尾部补充分的 event case：

`epoll_ctl04`, `epoll_ctl05`, `futex_cmp_requeue02`, `futex_wait02`,
`futex_wait04`, `pselect03`, `pselect03_64`。

`futex_cmp_requeue02` 在这次 LA musl/glibc focused run 中为 `3/3`，
修正此前单测记录的 `1/3`。

## 2026-05-26 failure notes

- TCONF: 25 recorded case(s); see per-case notes below.
- TBROK: 22 recorded case(s); see per-case notes below.
- EINVAL observed: 8 recorded case(s); see per-case notes below.
- TFAIL: 7 recorded case(s); see per-case notes below.
- host timeout before case completed: 1 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `epoll_create01` | 2/3 | partial | TCONF: syscall(-1) __NR_epoll_create not supported on your arch |
| `epoll_create02` | 0/3 | fail | TFAIL: epoll_create(0) invalid retval 3: SUCCESS (0) |
| `epoll_create1_01` | 2/2 | pass |  |
| `epoll_create1_02` | 2/2 | pass | EINVAL observed |
| `epoll_ctl01` | 3/3 | pass |  |
| `epoll_ctl02` | 9/9 | pass | EINVAL observed |
| `epoll_ctl03` | 256/256 | pass |  |
| `epoll_ctl04` | 1/1 | pass | EINVAL observed |
| `epoll_ctl05` | 1/1 | pass |  |
| `epoll_wait01` | 3/3 | pass |  |
| `epoll_wait02` | 7/7 | pass |  |
| `epoll_wait03` | 5/5 | pass | EINVAL observed |
| `epoll_wait04` | 0/1 | fail | TFAIL: epoll_wait() waited for 2382us with a timeout equal to zero |
| `epoll_wait06` | 9/9 | pass |  |
| `epoll_wait07` | 5/5 | pass |  |
| `eventfd01` | 4/4 | pass | EINVAL observed |
| `eventfd02` | 5/5 | pass | EINVAL observed |
| `eventfd03` | 3/3 | pass |  |
| `eventfd04` | 3/3 | pass |  |
| `eventfd05` | 2/2 | pass |  |
| `eventfd06` | 0/1 | skip | TCONF: libaio is not available |
| `eventfd2_01` | 2/2 | pass |  |
| `eventfd2_02` | 2/2 | pass |  |
| `eventfd2_03` | 2/2 | pass |  |
| `fanotify01` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify02` | 0/1 | skip | TCONF: fanotify is not configured in this kernel |
| `fanotify03` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify04` | 0/1 | skip | TCONF: fanotify is not configured in this kernel |
| `fanotify05` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify06` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify07` | 0/1 | skip | TCONF: fanotify is not configured in this kernel |
| `fanotify08` | 0/1 | skip | TCONF: fanotify is not configured in this kernel |
| `fanotify09` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify10` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify11` | 0/1 | skip | TCONF: fanotify not configured in kernel |
| `fanotify12` | 0/1 | skip | TCONF: fanotify not configured in kernel |
| `fanotify13` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify14` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify15` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify16` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify17` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify18` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify19` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify20` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify21` | 0/2 | fail | TBROK: Failed to acquire device |
| `fanotify22` | 0/1 | skip | TCONF: Couldn't find 'debugfs' in $PATH |
| `fanotify23` | 0/2 | fail | TBROK: Failed to acquire device |
| `futex_cmp_requeue01` | 0/0 | hang | single-case rerun still host-times out; multiple waiters are not woken/requeued and report `ETIMEDOUT` |
| `futex_cmp_requeue02` | 3/3 | pass | 2026-06-03 LA musl/glibc focused submit-tail rerun passes all Summary checks |
| `futex_wait01` | 4/4 | pass | single-case rerun exits cleanly; timeout and EAGAIN paths pass |
| `futex_wait02` | 1/1 | pass |  |
| `futex_wait03` | 1/1 | pass |  |
| `futex_wait04` | 1/1 | pass |  |
| `futex_wait05` | 7/7 | pass |  |
| `futex_wait_bitset01` | 2/2 | pass |  |
| `futex_waitv01` | 0/1 | skip | TCONF: syscall(-1) __NR_futex_waitv not supported on your arch |
| `futex_waitv02` | 0/2 | fail | TBROK: Test killed by SIGSEGV! |
| `futex_waitv03` | 0/1 | skip | TCONF: syscall(-1) __NR_futex_waitv not supported on your arch |
| `futex_wake01` | 6/6 | pass |  |
| `futex_wake02` | 0/1 | fail | TBROK: Failed to open FILE '/proc/26/task/27/stat' for reading: ENOENT (2) |
| `futex_wake03` | 11/11 | pass |  |
| `futex_wake04` | 0/1 | skip | TCONF: hugetlbfs is not supported |
| `inotify01` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify02` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify03` | 0/2 | fail | TBROK: Failed to acquire device |
| `inotify04` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify05` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify06` | 0/2 | fail | TBROK: Failed to open FILE '/proc/sys/fs/inotify/max_user_instances' for reading: ENOENT (2) |
| `inotify07` | 0/2 | fail | TBROK: Failed to acquire device |
| `inotify08` | 0/2 | fail | TBROK: Failed to acquire device |
| `inotify09` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify10` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify11` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify12` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify_init1_01` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `inotify_init1_02` | 0/1 | skip | TCONF: syscall(26) __NR_inotify_init1 not supported on your arch |
| `poll01` | 2/2 | pass |  |
| `poll02` | 7/7 | pass |  |
| `ppoll01` | 18/20 | partial | TFAIL: ret: 0, exp: -1, ret_errno: SUCCESS (0), exp_errno: EINTR (4) |
| `pselect01` | 1/7 | partial | TFAIL: pselect() woken up early 400 times range: [1985,1472] |
| `pselect01_64` | 1/7 | partial | TFAIL: pselect() woken up early 436 times range: [1741,1483] |
| `pselect02` | 3/3 | pass | EINVAL observed |
| `pselect02_64` | 3/3 | pass | EINVAL observed |
| `pselect03` | 1/1 | pass |  |
| `pselect03_64` | 1/1 | pass |  |
| `select01` | 6/13 | partial | TFAIL: select() with regular file timed out |
| `select02` | 14/17 | partial | TCONF: syscall(-1) __NR_select not supported on your arch |
| `select03` | 16/40 | partial | TCONF: syscall(-1) __NR_select not supported on your arch |
| `select04` | 4/7 | partial | single-case rerun exits cleanly; libc and pselect6 paths pass, select/time64/newselect variants are unsupported |
| `userfaultfd01` | 0/1 | fail | single-case rerun exits cleanly; `UFFDIO_API` ioctl on userfaultfd returns `EINVAL` |
