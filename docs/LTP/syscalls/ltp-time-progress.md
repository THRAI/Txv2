# LTP time Progress

`time` batch local tracking. Cases are from `tools/ltp-batches.py --batch time`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 55 | from `make ltp-batch-cases LTP_BATCH=time` |
| latest local run | focused time tails | 2026-05-30 direct QEMU tails covered through `times03` |
| cumulative scored | `176/200` prefix plus focused tails | broad prefix score is not recomputed across separate focused runs |
| reached case | `times03` | tail runs covered the cases after the broad timeout |
| logs | `target/oscomp/ltp-progress/time`, `target/oscomp/ltp-timeout-triage/time` | per-group stdout, single-case timeout triage logs, and serial snapshots |

## 2026-05-30 full-image prefix run

Direct non-Docker QEMU coverage with the full image reached `176/200` before
the 300s outer timeout stopped while `time01` was active. The run started 35
cases and completed 34; serial snapshot:
`target/oscomp/os_serial_out_ltp_time_partial_20260530_181533.txt`. Fault
decode found no kernel trap lines.

- Major improvement over the stale table: `adjtimex01/03`, `clock_adjtime01/02`,
  `clock_settime01`, `settimeofday01`, `stime01`, and `stime02` now reach real
  Linux-shaped behavior instead of `ENOSYS`.
- Passing prefix clusters: alarms, `clock_getres01`, `clock_gettime02`,
  `clock_nanosleep04`, `getitimer01/02`, `gettimeofday02`, `leapsec01`,
  `nanosleep04`, `setitimer02`, and basic realtime set/read paths.
- Remaining failures are narrower: missing `/proc/self/ns/time_for_children`,
  `clock_nanosleep` bad-pointer/restart/wait cleanup behavior,
  `clock_settime02/03` edge semantics, `gettimeofday01` timezone/bad-pointer
  handling, nanosleep timing/checkpoint cleanup, `setitimer01` cleanup, and
  `settimeofday02` blocked on unsupported `capget` capability probing.
- Verified with:
  `timeout 300s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/testdata --submit target/oscomp/submit --suite ltp-batch:time`;
  `python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata`;
  `cargo xtask fault-decode --target rv64-qemu --serial target/oscomp/os_serial_out_rv.txt --all --brief`.

## 2026-05-30 focused tail runs

The tail after `time01` was split into short `ltp-musl:...` selectors so the
guest command line did not silently drop later case names. Serial snapshots:

- `target/oscomp/os_serial_out_ltp_time_tail_20260530_190324.txt`: `42/121`,
  covering `time01` through `timerfd_gettime01`.
- `target/oscomp/os_serial_out_ltp_time_tail2_20260530_190543.txt`: `12/17`,
  covering `timerfd_settime01`, `times01`, and `times03`.

The focused tail confirms `time01`, `timer_delete02`, `timer_getoverrun01`,
`timer_gettime01`, `timerfd02`, `timerfd_create01`, `timerfd_gettime01`,
`timerfd_settime01`, and `times01` pass. `timer_create01..03` are absent from
the image or selector path (`sh: ... not found`). Remaining semantic blockers
are CPU-time POSIX timer support, timer-set errno ordering for null
`new_value`, `timer_settime03` cleanup timeout, timerfd tick delivery/counting,
time namespace `unshare(CLONE_NEWTIME)` for `timerfd04`, and child CPU
accounting in `times03`. Fault decode reported no kernel trap lines for both
tail logs.

## 2026-05-26 failure notes

- TBROK: 11 recorded case(s); see per-case notes below.
- EINVAL observed: 10 recorded case(s); see per-case notes below.
- TFAIL: 6 recorded case(s); see per-case notes below.
- host timeout before case completed: 2 recorded case(s); see per-case notes below.
- TCONF: 2 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `adjtimex01` | 2/2 | pass | 2026-05-30 full-image prefix passes |
| `adjtimex02` | 7/8 | partial | 2026-05-30 full-image prefix reaches real `EINVAL`/`EPERM`/`EFAULT` checks |
| `adjtimex03` | 1/1 | pass | 2026-05-30 full-image prefix passes |
| `alarm02` | 6/6 | pass |  |
| `alarm03` | 2/2 | pass |  |
| `alarm05` | 3/3 | pass |  |
| `alarm06` | 2/2 | pass |  |
| `alarm07` | 2/2 | pass |  |
| `clock_adjtime01` | 9/9 | pass | 2026-05-30 full-image prefix passes |
| `clock_adjtime02` | 6/6 | pass | 2026-05-30 full-image prefix passes |
| `clock_getres01` | 44/44 | pass |  |
| `clock_gettime01` | 0/0 | hang | single-case rerun still host-times out after entering libc/vDSO clock_gettime variant |
| `clock_gettime02` | 10/10 | pass | single-case rerun exits cleanly; invalid clock and bad pointer errno paths pass |
| `clock_gettime03` | 0/1 | fail | 2026-05-30 full-image prefix: missing `/proc/self/ns/time_for_children` |
| `clock_gettime04` | 0/0 | hang | single-case rerun still host-times out after several monotonic/realtime TPASS readings |
| `clock_nanosleep01` | 6/12 | partial | 2026-05-30 full-image prefix: restart/bad-pointer cases still mismatch |
| `clock_nanosleep02` | 0/4 | fail | 2026-05-30 full-image prefix: cleanup wait interrupted by `EINTR` |
| `clock_nanosleep03` | 0/1 | fail | 2026-05-30 full-image prefix: `unshare(CLONE_NEWTIME)` returns `ENOSYS` |
| `clock_nanosleep04` | 4/4 | pass |  |
| `clock_settime01` | 4/4 | pass | 2026-05-30 full-image prefix can advance and recede realtime |
| `clock_settime02` | 11/12 | partial | 2026-05-30 full-image prefix passes most invalid-clock cases; one edge remains |
| `clock_settime03` | 0/1 | fail | 2026-05-30 full-image prefix: cleanup wait interrupted by `EINTR` |
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
| `settimeofday01` | 1/1 | pass | 2026-05-30 full-image prefix passes |
| `settimeofday02` | 0/1 | skip | 2026-05-30 full-image prefix blocked by unsupported `capget` capability probing |
| `stime01` | 2/3 | partial | 2026-05-30 full-image prefix: libc/settimeofday variants pass; raw `stime` syscall unsupported on RV64 |
| `stime02` | 2/3 | partial | 2026-05-30 full-image prefix: non-root EPERM path passes for libc/settimeofday variants; raw `stime` syscall unsupported on RV64 |
| `time01` | 2/2 | pass |  |
| `timer_create01` | 0/0 | skip | 2026-05-30 focused tail: case binary not found in image/selector path |
| `timer_create02` | 0/0 | skip | 2026-05-30 focused tail: case binary not found in image/selector path |
| `timer_create03` | 0/0 | skip | 2026-05-30 focused tail: case binary not found in image/selector path |
| `timer_delete01` | 2/8 | partial | 2026-05-30 focused tail: realtime/monotonic delete paths pass; CPU-time clocks and several optional clocks unsupported |
| `timer_delete02` | 1/1 | pass | EINVAL observed |
| `timer_getoverrun01` | 2/2 | pass | EINVAL observed |
| `timer_gettime01` | 3/3 | pass | EINVAL observed |
| `timer_settime01` | 8/32 | partial | 2026-05-30 focused tail: realtime/monotonic pass; CPU-time clocks fail `timer_create`, optional clocks TCONF |
| `timer_settime02` | 10/48 | partial | 2026-05-30 focused tail: null `new_value` returns `EFAULT` where LTP expects `EINVAL`; CPU-time clocks unsupported |
| `timer_settime03` | 0/1 | fail | 2026-05-30 focused tail: timeout cleanup wait interrupted by `EINTR` |
| `timerfd01` | 3/12 | partial | 2026-05-30 focused tail: relative readback passes, but tick delivery/counting remains wrong |
| `timerfd02` | 6/6 | pass |  |
| `timerfd04` | 0/1 | fail | 2026-05-30 focused tail: `unshare(CLONE_NEWTIME)` returns `ENOSYS` |
| `timerfd_create01` | 2/2 | pass | EINVAL observed |
| `timerfd_gettime01` | 3/3 | pass | EINVAL observed |
| `timerfd_settime01` | 4/4 | pass | EINVAL observed |
| `times01` | 1/1 | pass |  |
| `times03` | 7/12 | partial | 2026-05-30 focused tail: child `cutime`/`cstime` accounting remains zero |
