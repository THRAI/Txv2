# LTP sched Progress

`sched` batch local tracking. Cases are from `tools/ltp-batches.py --batch sched`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 64 | from `make ltp-batch-cases LTP_BATCH=sched` |
| latest local run | getrlimit03 | 2026-05-26 RV/LA rerun passes 16/16 |
| cumulative scored | `73/187` | recorded rows in this document; previous `1878` maxima for `sched_getattr02`/`sched_setattr01` were stale batch-parser artifacts, single-case judge reports `4/4` each |
| reached case | `setrlimit06` | batch completed |
| logs | `target/oscomp/ltp-progress/sched`, `target/oscomp/ltp-timeout-triage/sched` | per-group stdout, single-case timeout triage logs, and serial snapshots |

## 2026-05-26 failure notes

- TFAIL: 21 recorded case(s); see per-case notes below.
- TBROK: 10 recorded case(s); see per-case notes below.
- TCONF: 15 recorded case(s); see per-case notes below.
- host timeout before case completed: 2 recorded case(s); see per-case notes below.
- EINVAL observed: 1 recorded case(s); see per-case notes below.
- `getrlimit(2)` old generic ABI now mirrors `prlimit64(pid=0, old_rlim)`; `getrlimit03` passes all 16 resource comparisons on RV/LA.

## 2026-06-04 priority/prctl focused rerun

Focused RV/LA musl reruns were made without `LTP_MAX_RUNTIME`; the earlier
`-I 25` runs are not used for scoring because `-I` makes LTP loop for the
given interval and inflates `Summary: passed`.

Logs:

- `target/oscomp/ltp-sched-prctl-a-rv-musl-noI-20260604.txt`
- `target/oscomp/ltp-sched-prctl-a-la-musl-noI-20260603.txt`

Correct no-`-I` result on both RV and LA: `41/51`.

Stable full-pass submit candidates from this slice: `getpriority01`,
`getpriority02`, `nice01`, `nice02`, `nice03`, `nice04`, `prctl01`,
and `prctl09` (`22/22` per arch/libc lane when counted only as full-pass
rows). `prctl02` (`10/18`) and `prctl08` (`9/11`, case return nonzero) are
recorded as partial only and should not be promoted under the full-pass-only
rule.

## 2026-06-04 sched priority focused rerun

Focused RV/LA musl reruns were made without `LTP_MAX_RUNTIME`.

Logs:

- `target/oscomp/ltp-sched-extra-a-rv-musl-noI-20260604.txt`
- `target/oscomp/ltp-sched-extra-a-la-musl-noI-20260604.txt`
- `target/oscomp/ltp-sched-extra-b-rv-musl-noI-20260604.txt`
- `target/oscomp/ltp-sched-extra-b-la-musl-noI-20260604.txt`
- RV glibc focused candidate run: `target/oscomp/os_serial_out_rv.txt`

Stable full-pass submit candidates from this slice are
`sched_get_priority_max01`, `sched_get_priority_max02`,
`sched_get_priority_min01`, `sched_get_priority_min02`,
`sched_rr_get_interval01`, and `setpriority02` (`29/29` per lane). RV also
passes several `sched_setparam*` rows, but LA musl reports `TCONF` for libc
variants and `sched_setscheduler04` fails, so those rows stay out of the
default submit whitelist.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `getcpu01` | 0/1 | skip | TCONF: syscall(168) __NR_getcpu not supported on your arch |
| `getpriority01` | 3/3 | pass | RV/LA musl no-`-I` focused rerun passes |
| `getpriority02` | 4/4 | pass | RV/LA musl no-`-I` focused rerun passes |
| `getrlimit01` | 16/16 | pass |  |
| `getrlimit02` | 2/2 | pass | EINVAL observed |
| `getrlimit03` | 16/16 | pass | old `getrlimit(2)` ABI returns the same rlimit table as `prlimit64`; RV/LA pass |
| `getrusage01` | 2/2 | pass |  |
| `getrusage02` | 3/4 | partial | TCONF: EFAULT is skipped for libc variant |
| `getrusage03` | 0/0 | hang | single-case rerun still host-times out; before hang it reports missing `/proc/self/status` and one `TPASS` for child/self usage comparison |
| `getrusage04` | 0/0 | hang | single-case rerun still host-times out while printing repeated usage timer accounting |
| `ioprio_get01` | 0/1 | skip | TCONF: syscall(31) __NR_ioprio_get not supported on your arch |
| `ioprio_set01` | 0/1 | skip | TCONF: syscall(31) __NR_ioprio_get not supported on your arch |
| `ioprio_set02` | 0/1 | skip | TCONF: syscall(30) __NR_ioprio_set not supported on your arch |
| `ioprio_set03` | 0/1 | skip | TCONF: syscall(30) __NR_ioprio_set not supported on your arch |
| `membarrier01` | 3/4 | partial | TBROK: Test 3 haven't reported results! |
| `nice01` | 3/3 | pass | RV/LA musl no-`-I` focused rerun passes |
| `nice02` | 1/1 | pass | RV/LA musl no-`-I` focused rerun passes |
| `nice03` | 1/1 | pass | RV/LA musl no-`-I` focused rerun passes |
| `nice04` | 1/1 | pass | RV/LA musl no-`-I` focused rerun passes |
| `nice05` | 0/2 | fail | TBROK: getpriority(0, 0) failed: ENOSYS (38) |
| `prctl01` | 2/2 | pass | RV/LA musl no-`-I` focused rerun passes |
| `prctl02` | 10/18 | partial | RV/LA musl no-`-I` focused rerun; partial only, not a full-pass promotion candidate |
| `prctl03` | 0/1 | fail | TFAIL: prctl(PR_SET_CHILD_SUBREAPER) failed: ENOSYS (38) |
| `prctl04` | 0/1 | fail | TBROK: current environment doesn't permit PR_GET/SET_SECCOMP: ENOSYS (38) |
| `prctl05` | 0/2 | fail | TFAIL: prctl(PR_SET_NAME) failed: ENOSYS (38) |
| `prctl06` | 0/2 | fail | TBROK: Failed to acquire device |
| `prctl07` | 0/1 | fail | TBROK: current environment doesn't permit PR_CAP_AMBIENT: ENOSYS (38) |
| `prctl08` | 9/11 | partial | RV/LA musl no-`-I` focused rerun; case returns nonzero, not a full-pass promotion candidate |
| `prctl09` | 7/7 | pass | RV/LA musl no-`-I` focused rerun passes |
| `prctl10` | 0/1 | skip | TCONF: This arch 'unknown' is not supported for test! |
| `sched_get_priority_max01` | 6/6 | pass | RV/LA musl no-`-I` focused rerun passes; RV glibc focused candidate passes |
| `sched_get_priority_max02` | 1/1 | pass | RV/LA musl no-`-I` focused rerun passes; RV glibc focused candidate passes |
| `sched_get_priority_min01` | 6/6 | pass | RV/LA musl no-`-I` focused rerun passes; RV glibc focused candidate passes |
| `sched_get_priority_min02` | 1/1 | pass | RV/LA musl no-`-I` focused rerun passes; RV glibc focused candidate passes |
| `sched_getaffinity01` | 4/4 | pass | `/proc/sys/kernel/pid_max` projection added; invalid pid now reaches ESRCH path |
| `sched_getattr01` | 1/1 | pass | deadline attributes are stored per tid by the validation-only `sched_setattr` shim and read back by `sched_getattr` |
| `sched_getattr02` | 4/4 | pass | single-case run passes ESRCH/EINVAL validation after adding `/proc/sys/kernel/pid_max` and minimal `sched_getattr` ABI |
| `sched_getparam01` | 0/4 | skip | single-case rerun exits cleanly; libc and syscall variants report `sched_getparam` unsupported |
| `sched_getparam03` | 0/2 | skip | single-case rerun exits cleanly; libc and syscall variants report `sched_getparam` unsupported |
| `sched_getscheduler01` | 0/2 | skip | single-case rerun exits cleanly; libc and syscall variants report `sched_getscheduler` unsupported |
| `sched_getscheduler02` | 0/2 | skip | TCONF: `sched_getscheduler` syscall/libc path unsupported after pid_max unblock |
| `sched_rr_get_interval01` | 4/4 | pass | RV/LA musl no-`-I` focused rerun passes; RV glibc focused candidate passes |
| `sched_rr_get_interval02` | 0/2 | fail | TFAIL: sched_rr_get_interval() returned -1, tp.tv_sec = 99, tp.tv_nsec = 99: ENOSYS (38) |
| `sched_rr_get_interval03` | 0/4 | fail | pid_max unblock exposes real gap: `sched_rr_get_interval` returns ENOSYS instead of EINVAL/ESRCH |
| `sched_setaffinity01` | 4/4 | pass | added cross-process EPERM check while keeping same-process/thread affinity behavior |
| `sched_setattr01` | 4/4 | pass | single-case run passes success/ESRCH/EINVAL validation after adding `/proc/sys/kernel/pid_max` and minimal `sched_setattr` ABI |
| `sched_setparam01` | 0/2 | skip | TCONF: sched_setparam not supported |
| `sched_setparam02` | 0/2 | skip | single-case rerun exits cleanly; libc and syscall variants report `sched_setparam` unsupported |
| `sched_setparam03` | 0/4 | skip | single-case rerun exits cleanly; `sched_setparam`/`sched_getparam` unsupported |
| `sched_setparam04` | 0/2 | skip | single-case rerun exits cleanly; libc and syscall variants report `sched_setparam` unsupported |
| `sched_setparam05` | 0/2 | skip | TCONF: sched_setparam not supported |
| `sched_setscheduler01` | 8/8 | pass | replaced unconditional success stub with pid/policy/param/priority validation; scheduler policy is still not actually changed |
| `sched_setscheduler02` | 0/2 | fail | TFAIL: sched_setscheduler(0, SCHED_FIFO, 1) succeeded |
| `sched_setscheduler03` | 0/1 | fail | TBROK: Expect rlim_cur = 19, get 18446744073709551615: ENOENT (2) |
| `sched_setscheduler04` | 0/8 | fail | TFAIL: Policy NOT reset to SCHED_NORMAL |
| `sched_yield01` | 0/1 | fail | TFAIL: sched_yield01.c:72: call failed - errno 38 : Function not implemented |
| `setpriority01` | 0/1 | fail | TBROK: getpwnam(ltp_setpriority01) failed: ENOENT (2) |
| `setpriority02` | 7/7 | pass | RV/LA musl no-`-I` focused rerun passes; RV glibc focused candidate passes |
| `setrlimit01` | 2/4 | partial | TFAIL: setrlimit01.c:183: setrlimit failed, expected 10 got 26 |
| `setrlimit02` | 1/2 | partial | TFAIL: call succeeded unexpectedly |
| `setrlimit03` | 1/2 | partial | TFAIL: call succeeded unexpectedly (nr_open=1048576 rlim_cur=1024 rlim_max=1048577) |
| `setrlimit04` | 1/1 | pass |  |
| `setrlimit05` | 1/1 | pass |  |
| `setrlimit06` | 0/1 | fail | TFAIL: Got no signal after reaching both limit |
