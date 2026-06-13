# LTP time Progress

`time` batch local tracking. Cases are from `tools/ltp-batches.py --batch time`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 55 | from `make ltp-batch-cases LTP_BATCH=time` |
| latest local run | focused LA64 submit-tail rerun | 2026-06-03 promoted whitelist candidates, musl+glibc |
| cumulative scored | `284/351` | recorded rows in this document |
| reached case | `times03` | batch completed |
| logs | `target/oscomp/ltp-progress/time`, `target/oscomp/ltp-timeout-triage/time` | per-group stdout, single-case timeout triage logs, and serial snapshots |

## 2026-06-03 focused submit-tail rerun

复测日志：

- LA musl: `target/oscomp/ltp-extra-core-b1-la-musl-20260603.txt`
- LA glibc: `target/oscomp/ltp-extra-core-g3-la-glibc-20260603.txt`

确认可作为 active submit 尾部补充分的 time case：

`gettimeofday02`, `timer_delete02`, `timer_settime03`, `times01`。

这些 case 在 LA musl/glibc focused run 中均为 Summary 满分。

## 2026-05-26 failure notes

- TBROK: 11 recorded case(s); see per-case notes below.
- EINVAL observed: 10 recorded case(s); see per-case notes below.
- TFAIL: 6 recorded case(s); see per-case notes below.
- host timeout before case completed: 2 recorded case(s); see per-case notes below.
- TCONF: 2 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `adjtimex01` | 0/1 | fail | TBROK: adjtimex(): failed to save current params: ENOSYS (38) |
| `adjtimex02` | 0/2 | fail | TBROK: adjtimex(): failed to save current params: ENOSYS (38) |
| `adjtimex03` | 0/1 | fail | TBROK: adjtimex(): Unexpeceted error, expecting EINVAL with mode 0x8000: ENOSYS (38) |
| `alarm02` | 6/6 | pass |  |
| `alarm03` | 2/2 | pass |  |
| `alarm05` | 3/3 | pass |  |
| `alarm06` | 2/2 | pass |  |
| `alarm07` | 2/2 | pass |  |
| `clock_adjtime01` | 0/4 | fail | TBROK: tst_clock_settime() realtime failed: ENOSYS (38) |
| `clock_adjtime02` | 0/4 | fail | TBROK: tst_clock_settime() realtime failed: ENOSYS (38) |
| `clock_getres01` | 44/44 | pass |  |
| `clock_gettime01` | 0/0 | hang | single-case rerun still host-times out after entering libc/vDSO clock_gettime variant |
| `clock_gettime02` | 10/10 | pass | single-case rerun exits cleanly; invalid clock and bad pointer errno paths pass |
| `clock_gettime03` | 0/1 | fail | single-case rerun exits cleanly; missing `/proc/self/ns/time_for_children` |
| `clock_gettime04` | 0/0 | hang | single-case rerun still host-times out after several monotonic/realtime TPASS readings |
| `clock_nanosleep01` | 11/14 | partial | TFAIL: returned 0, expected -1, expected errno: EFAULT (14): SUCCESS (0) |
| `clock_nanosleep02` | 7/7 | pass |  |
| `clock_nanosleep03` | 0/2 | skip | TCONF: unshare(128) unsupported: EINVAL (22) |
| `clock_nanosleep04` | 4/4 | pass |  |
| `clock_settime01` | 0/6 | fail | TBROK: tst_clock_settime() realtime failed: ENOSYS (38) |
| `clock_settime02` | 0/4 | fail | TBROK: tst_clock_settime() realtime failed: ENOSYS (38) |
| `clock_settime03` | 0/4 | fail | TBROK: tst_clock_settime() realtime failed: ENOSYS (38) |
| `getitimer01` | 30/30 | pass |  |
| `getitimer02` | 3/3 | pass | EINVAL observed |
| `gettimeofday01` | 2/3 | partial | TFAIL: tst_syscall(__NR_gettimeofday, tc->tv, tc->tz) succeeded |
| `gettimeofday02` | 1/1 | pass |  |
| `leapsec01` | 0/2 | fail | TBROK: clock_gettime(CLOCK_REALTIME) failed: ENOSYS (38) |
| `nanosleep01` | 7/7 | pass |  |
| `nanosleep02` | 2/2 | pass |  |
| `nanosleep04` | 3/3 | pass | EINVAL observed |
| `setitimer01` | 18/18 | pass |  |
| `setitimer02` | 3/3 | pass | EINVAL observed |
| `settimeofday01` | 0/4 | fail | TBROK: tst_clock_settime() realtime failed: ENOSYS (38) |
| `settimeofday02` | 1/3 | partial | TFAIL: settimeofday(&tc->tv, NULL) expected EINVAL: ENOSYS (38) |
| `stime01` | 0/8 | fail | TBROK: tst_clock_settime() realtime failed: ENOSYS (38) |
| `stime02` | 0/3 | fail | TFAIL: stime(2) fails, Caller not root, expected errno:1: ENOSYS (38) |
| `time01` | 2/2 | pass |  |
| `timer_create01` | 0/0 | skip |  |
| `timer_create02` | 0/0 | skip |  |
| `timer_create03` | 0/0 | skip |  |
| `timer_delete01` | 8/8 | pass |  |
| `timer_delete02` | 1/1 | pass | EINVAL observed |
| `timer_getoverrun01` | 2/2 | pass | EINVAL observed |
| `timer_gettime01` | 3/3 | pass | EINVAL observed |
| `timer_settime01` | 32/32 | pass |  |
| `timer_settime02` | 48/48 | pass | EINVAL observed |
| `timer_settime03` | 1/1 | pass |  |
| `timerfd01` | 3/12 | partial | TFAIL: no ticks happened |
| `timerfd02` | 6/6 | pass |  |
| `timerfd04` | 0/1 | skip | TCONF: unshare(128) unsupported: EINVAL (22) |
| `timerfd_create01` | 2/2 | pass | EINVAL observed |
| `timerfd_gettime01` | 3/3 | pass | EINVAL observed |
| `timerfd_settime01` | 4/4 | pass | EINVAL observed |
| `times01` | 1/1 | pass |  |
| `times03` | 7/12 | partial | TFAIL: buf1.tms_utime = 2170 |
