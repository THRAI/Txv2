# LTP event Progress

`event` batch local tracking. Cases are from `tools/ltp-batches.py --batch event`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 90 | from `make ltp-batch-cases LTP_BATCH=event` |
| latest local run | focused event tails | 2026-05-30 direct QEMU tails covered through `userfaultfd01` |
| cumulative scored | `331/385` prefix plus focused tails | broad prefix score is not recomputed across separate focused runs |
| reached case | `userfaultfd01` | tail runs covered the cases after the broad timeout |
| logs | `target/oscomp/ltp-progress/event`, `target/oscomp/ltp-timeout-triage/event` | per-group stdout, single-case timeout triage logs, and serial snapshots |

## 2026-05-30 full-image prefix run

Direct non-Docker QEMU coverage with the full image reached `331/385` before
the 300s outer timeout stopped while `futex_wait_bitset01` was active. The run
started 54 cases and completed 53; serial snapshot:
`target/oscomp/os_serial_out_ltp_event_partial_20260530_180900.txt`. Fault
decode found no kernel trap lines.

- Passing clusters in the fresh prefix: most `epoll_ctl*`, `epoll_wait01/03/06/07`,
  `eventfd*`, `fanotify08`, and basic `futex_wait01..04`.
- Epoll gaps: legacy `epoll_create` and `epoll_create(0)` semantics,
  nested epoll error (`ELOOP` expected, `EINVAL` observed), and timing-sensitive
  `epoll_wait02/04` sleeps.
- Fanotify remains mostly policy/device setup: many cases still fail on
  `test_dev.img` acquisition or unsupported/configuration paths.
- Futex gaps: `FUTEX_CMP_REQUEUE` invalid cases still succeed unexpectedly,
  and `futex_wait05` times out in cleanup before the run reaches
  `futex_wait_bitset01`.
- Verified with:
  `timeout 300s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/testdata --submit target/oscomp/submit --suite ltp-batch:event`;
  `python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata`;
  `cargo xtask fault-decode --target rv64-qemu --serial target/oscomp/os_serial_out_rv.txt --all --brief`.

## 2026-05-30 focused tail runs

The timeout tail was split into short `ltp-musl:...` selectors to avoid long
kernel command-line truncation. The first long selector reached a bogus
truncated `inotif` command, so later runs kept selectors shorter. Serial
snapshots:

- `target/oscomp/os_serial_out_ltp_event_tail_20260530_185920.txt`: `17/44`,
  from `futex_wait_bitset01` through `inotify11`, then truncated to `inotif`.
- `target/oscomp/os_serial_out_ltp_event_tail2_20260530_190138.txt`: `54/90`,
  from `inotify12` through `select02`; host timeout occurred after starting
  `select03`.
- `target/oscomp/os_serial_out_ltp_event_tail3_20260530_190504.txt`: `16/42`,
  covering `select03`, `select04`, and `userfaultfd01`.

The tails confirm `futex_wake01/03`, `inotify_init1_01/02`, `poll01`,
`pselect02*`, `pselect03*`, and `select03` now reach passing paths. Remaining
tail blockers are timer accuracy and signal-interrupt races in
`poll02`/`ppoll01`/`pselect01*`/`select01..02`, missing inotify watch/remove
syscalls and `/proc/sys/fs/inotify/*`, missing `/proc/<pid>/task/<tid>/stat`
for futex helper accounting, `select04` cleanup timeout, and `UFFDIO_API`
returning `EINVAL`. Fault decode reported no kernel trap lines for the final
tail logs.

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
| `epoll_ctl05` | 0/1 | fail | 2026-05-30 full-image prefix: nested epoll returns `EINVAL`, expected `ELOOP` |
| `epoll_wait01` | 3/3 | pass |  |
| `epoll_wait02` | 0/3 | fail | 2026-05-30 full-image prefix: timing run slept too long |
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
| `fanotify08` | 2/2 | pass | 2026-05-30 full-image prefix passes |
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
| `futex_cmp_requeue02` | 1/3 | partial | single-case rerun exits cleanly; invalid `FUTEX_CMP_REQUEUE` cases succeed unexpectedly, EAGAIN path passes |
| `futex_wait01` | 4/4 | pass | single-case rerun exits cleanly; timeout and EAGAIN paths pass |
| `futex_wait02` | 1/1 | pass |  |
| `futex_wait03` | 1/1 | pass |  |
| `futex_wait04` | 1/1 | pass |  |
| `futex_wait05` | 0/1 | fail | 2026-05-30 full-image prefix: LTP timeout cleanup wait interrupted |
| `futex_wait_bitset01` | 0/2 | fail | 2026-05-30 focused tail: timeout path waited too long, then cleanup wait saw `EINTR` |
| `futex_waitv01` | 0/1 | skip | TCONF: syscall(-1) __NR_futex_waitv not supported on your arch |
| `futex_waitv02` | 0/2 | fail | TBROK: Test killed by SIGSEGV! |
| `futex_waitv03` | 0/1 | skip | TCONF: syscall(-1) __NR_futex_waitv not supported on your arch |
| `futex_wake01` | 6/6 | pass |  |
| `futex_wake02` | 0/1 | fail | 2026-05-30 focused tail: missing `/proc/<pid>/task/<tid>/stat` |
| `futex_wake03` | 11/11 | pass |  |
| `futex_wake04` | 0/1 | skip | TCONF: hugetlbfs is not supported |
| `inotify01` | 0/1 | skip | 2026-05-30 focused tail: `inotify_add_watch` unsupported |
| `inotify02` | 0/1 | skip | 2026-05-30 focused tail: `inotify_add_watch` unsupported |
| `inotify03` | 0/2 | fail | TBROK: Failed to acquire device |
| `inotify04` | 0/1 | skip | 2026-05-30 focused tail: `inotify_add_watch` unsupported |
| `inotify05` | 0/3 | skip | 2026-05-30 focused tail: `inotify_add_watch` / `inotify_rm_watch` unsupported |
| `inotify06` | 0/2 | fail | TBROK: Failed to open FILE '/proc/sys/fs/inotify/max_user_instances' for reading: ENOENT (2) |
| `inotify07` | 0/2 | fail | TBROK: Failed to acquire device |
| `inotify08` | 0/2 | fail | TBROK: Failed to acquire device |
| `inotify09` | 0/3 | skip | 2026-05-30 focused tail: `inotify_add_watch` unsupported; follow-on fd use warns |
| `inotify10` | 0/1 | skip | 2026-05-30 focused tail: `inotify_add_watch` unsupported |
| `inotify11` | 0/1 | skip | 2026-05-30 focused tail: `inotify_add_watch` unsupported |
| `inotify12` | 0/1 | skip | 2026-05-30 focused tail: `inotify_add_watch` unsupported |
| `inotify_init1_01` | 4/4 | pass | 2026-05-30 focused tail passes CLOEXEC flag checks |
| `inotify_init1_02` | 4/4 | pass | 2026-05-30 focused tail passes NONBLOCK flag checks |
| `poll01` | 2/2 | pass |  |
| `poll02` | 5/7 | partial | 2026-05-30 focused tail: timer accuracy subcases slept too long |
| `ppoll01` | 18/20 | partial | TFAIL: ret: 0, exp: -1, ret_errno: SUCCESS (0), exp_errno: EINTR (4) |
| `pselect01` | 1/7 | partial | TFAIL: pselect() woken up early 400 times range: [1985,1472] |
| `pselect01_64` | 1/7 | partial | TFAIL: pselect() woken up early 436 times range: [1741,1483] |
| `pselect02` | 3/3 | pass | EINVAL observed |
| `pselect02_64` | 3/3 | pass | EINVAL observed |
| `pselect03` | 1/1 | pass |  |
| `pselect03_64` | 1/1 | pass |  |
| `select01` | 6/13 | partial | TFAIL: select() with regular file timed out |
| `select02` | 14/17 | partial | TCONF: syscall(-1) __NR_select not supported on your arch |
| `select03` | 16/40 | partial | 2026-05-30 focused tail exits cleanly; libc and pselect6 variants pass, unsupported select/time64/newselect variants TCONF |
| `select04` | 0/1 | fail | 2026-05-30 focused tail: libc select timeout cleanup wait interrupted by `EINTR` |
| `userfaultfd01` | 0/1 | fail | 2026-05-30 focused tail: `UFFDIO_API` ioctl on userfaultfd returns `EINVAL` |
