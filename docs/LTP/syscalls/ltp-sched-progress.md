# LTP sched Progress

`sched` batch local tracking. Cases are from `tools/ltp-batches.py --batch sched`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 64 | from `make ltp-batch-cases LTP_BATCH=sched` |
| latest local run | full-image sched batch | 2026-05-30 direct QEMU run scored `112/219` |
| cumulative scored | `112/219` | latest full-image batch score; previous `1878` maxima for `sched_getattr02`/`sched_setattr01` were stale batch-parser artifacts |
| reached case | `setrlimit06` | batch completed |
| logs | `target/oscomp/ltp-progress/sched`, `target/oscomp/ltp-timeout-triage/sched` | per-group stdout, single-case timeout triage logs, and serial snapshots |

## 2026-05-30 full-image batch run

Direct non-Docker QEMU coverage with the full image completed the `sched`
batch and userspace exited cleanly. The run started and completed 62 cases;
serial snapshot: `target/oscomp/os_serial_out_ltp_sched_full_20260530_180055.txt`.
Local judge score: `112/219`.

- Passing clusters: `getcpu01`, `getrlimit01..03`, `getrusage01/02`,
  `ioprio_get01`, `sched_get_priority_*02`, `sched_getaffinity01`,
  `sched_getattr01/02`, `sched_getscheduler02`, `sched_rr_get_interval02`,
  `sched_setaffinity01`, `sched_setattr01`, `sched_setparam01`,
  `sched_setscheduler01`, `sched_yield01`, and `setrlimit04/05`.
- Wired-but-incomplete clusters now exposed by the full-image run:
  `getpriority`/`setpriority`/`nice` credential and errno semantics,
  ioprio setters, `PR_SET_NAME` procfs `task/<tid>/comm`, timer slack
  `prctl`, scheduler priority ranges for FIFO/RR/BATCH/IDLE/DEADLINE,
  `sched_getparam` child/thread lookup, `sched_setparam` priority
  preservation, and `RLIMIT_CPU` signal delivery.
- Harness/policy cases: `prctl06` still fails test-device acquisition;
  seccomp, ambient capabilities, unsupported arch, and old scheduler feature
  cases remain TCONF/skip-shaped.
- Verified with:
  `timeout 300s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/testdata --submit target/oscomp/submit --suite ltp-batch:sched`
  and
  `python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata`.

## 2026-05-26 failure notes

- TFAIL: 21 recorded case(s); see per-case notes below.
- TBROK: 10 recorded case(s); see per-case notes below.
- TCONF: 15 recorded case(s); see per-case notes below.
- host timeout before case completed: 2 recorded case(s); see per-case notes below.
- EINVAL observed: 1 recorded case(s); see per-case notes below.
- `getrlimit(2)` old generic ABI now mirrors `prlimit64(pid=0, old_rlim)`; `getrlimit03` passes all 16 resource comparisons on RV/LA.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `getcpu01` | 1/1 | pass | 2026-05-30 full-image batch passes |
| `getpriority01` | 1/3 | partial | 2026-05-30 full-image batch reaches real priority semantics instead of ENOSYS |
| `getpriority02` | 2/4 | partial | 2026-05-30 full-image batch reaches real priority errno semantics instead of ENOSYS |
| `getrlimit01` | 16/16 | pass |  |
| `getrlimit02` | 2/2 | pass | EINVAL observed |
| `getrlimit03` | 16/16 | pass | old `getrlimit(2)` ABI returns the same rlimit table as `prlimit64`; RV/LA pass |
| `getrusage01` | 2/2 | pass |  |
| `getrusage02` | 3/4 | partial | TCONF: EFAULT is skipped for libc variant |
| `getrusage03` | 0/0 | hang | single-case rerun still host-times out; before hang it reports missing `/proc/self/status` and one `TPASS` for child/self usage comparison |
| `getrusage04` | 0/0 | hang | single-case rerun still host-times out while printing repeated usage timer accounting |
| `ioprio_get01` | 1/1 | pass | 2026-05-30 full-image batch returns BEST-EFFORT priority 0 |
| `ioprio_set01` | 0/2 | partial | 2026-05-30 full-image batch: setter returns `EINVAL`; priority decrease is TCONF |
| `ioprio_set02` | 0/16 | fail | 2026-05-30 full-image batch: BE/IDLE priority changes rejected and class remains BEST-EFFORT |
| `ioprio_set03` | 2/3 | partial | 2026-05-30 full-image batch reaches real ioprio setter checks |
| `membarrier01` | 3/4 | partial | TBROK: Test 3 haven't reported results! |
| `nice01` | 0/1 | fail | TBROK: getpriority(0, 0) failed: ENOSYS (38) |
| `nice02` | 0/1 | fail | TFAIL: nice(50) returned -1: ENOSYS (38) |
| `nice03` | 0/1 | fail | TBROK: getpriority(0, 0) failed: ENOSYS (38) |
| `nice04` | 0/1 | fail | TFAIL: nice(-10) should fail with EPERM: ENOSYS (38) |
| `nice05` | 0/2 | fail | TBROK: getpriority(0, 0) failed: ENOSYS (38) |
| `prctl01` | 0/1 | fail | TFAIL: prctl(PR_SET_PDEATHSIG) failed: ENOSYS (38) |
| `prctl02` | 0/18 | fail | TFAIL: prctl() failed unexpectedly, expected EINVAL: ENOSYS (38) |
| `prctl03` | 0/1 | fail | TFAIL: prctl(PR_SET_CHILD_SUBREAPER) failed: ENOSYS (38) |
| `prctl04` | 0/1 | fail | TBROK: current environment doesn't permit PR_GET/SET_SECCOMP: ENOSYS (38) |
| `prctl05` | 2/3 | partial | 2026-05-30 full-image batch: PR_SET/GET_NAME pass, `/proc/self/task/<tid>/comm` is missing |
| `prctl06` | 0/2 | fail | TBROK: Failed to acquire device |
| `prctl07` | 0/1 | fail | TBROK: current environment doesn't permit PR_CAP_AMBIENT: ENOSYS (38) |
| `prctl08` | 0/6 | fail | TFAIL: prctl(PR_SET_TIMERSLACK, 0) failed: ENOSYS (38) |
| `prctl09` | 0/1 | fail | TBROK: prctl set timerslack 200us failed: ENOSYS (38) |
| `prctl10` | 0/1 | skip | TCONF: This arch 'unknown' is not supported for test! |
| `sched_get_priority_max01` | 1/6 | partial | 2026-05-30 full-image batch: SCHED_OTHER passes; FIFO/RR/BATCH/IDLE/DEADLINE ranges still return `EINVAL` |
| `sched_get_priority_max02` | 1/1 | pass | 2026-05-30 full-image batch passes invalid-policy `EINVAL` |
| `sched_get_priority_min01` | 1/6 | partial | 2026-05-30 full-image batch: SCHED_OTHER passes; other policy ranges still return `EINVAL` |
| `sched_get_priority_min02` | 1/1 | pass | 2026-05-30 full-image batch passes invalid-policy `EINVAL` |
| `sched_getaffinity01` | 4/4 | pass | `/proc/sys/kernel/pid_max` projection added; invalid pid now reaches ESRCH path |
| `sched_getattr01` | 1/1 | pass | deadline attributes are stored per tid by the validation-only `sched_setattr` shim and read back by `sched_getattr` |
| `sched_getattr02` | 4/4 | pass | single-case run passes ESRCH/EINVAL validation after adding `/proc/sys/kernel/pid_max` and minimal `sched_getattr` ABI |
| `sched_getparam01` | 2/4 | partial | 2026-05-30 full-image batch: self priority passes; child/thread lookup returns `ESRCH` |
| `sched_getparam03` | 2/6 | partial | 2026-05-30 full-image batch reaches real `sched_getparam` behavior |
| `sched_getscheduler01` | 2/6 | partial | 2026-05-30 full-image batch reaches real `sched_getscheduler` behavior |
| `sched_getscheduler02` | 2/2 | pass | 2026-05-30 full-image batch passes |
| `sched_rr_get_interval01` | 2/4 | partial | 2026-05-30 full-image batch wired; remaining policy cases mismatch |
| `sched_rr_get_interval02` | 2/2 | pass | 2026-05-30 full-image batch passes |
| `sched_rr_get_interval03` | 3/6 | partial | 2026-05-30 full-image batch: `pid=-1` returns `ESRCH`, expected `EINVAL` |
| `sched_setaffinity01` | 4/4 | pass | added cross-process EPERM check while keeping same-process/thread affinity behavior |
| `sched_setattr01` | 4/4 | pass | single-case run passes success/ESRCH/EINVAL validation after adding `/proc/sys/kernel/pid_max` and minimal `sched_setattr` ABI |
| `sched_setparam01` | 2/2 | pass | 2026-05-30 full-image batch passes priority-zero case |
| `sched_setparam02` | 2/10 | partial | 2026-05-30 full-image batch: nonzero priority changes rejected and not preserved |
| `sched_setparam03` | 0/4 | fail | 2026-05-30 full-image batch reaches real setter behavior |
| `sched_setparam04` | 4/8 | partial | 2026-05-30 full-image batch reaches real setter behavior |
| `sched_setparam05` | 0/2 | fail | 2026-05-30 full-image batch reaches real setter behavior |
| `sched_setscheduler01` | 8/8 | pass | replaced unconditional success stub with pid/policy/param/priority validation; scheduler policy is still not actually changed |
| `sched_setscheduler02` | 0/2 | fail | TFAIL: sched_setscheduler(0, SCHED_FIFO, 1) succeeded |
| `sched_setscheduler03` | 0/1 | fail | TBROK: Expect rlim_cur = 19, get 18446744073709551615: ENOENT (2) |
| `sched_setscheduler04` | 0/8 | fail | TFAIL: Policy NOT reset to SCHED_NORMAL |
| `sched_yield01` | 1/1 | pass | 2026-05-30 full-image batch passes |
| `setpriority01` | 0/1 | fail | TBROK: getpwnam(ltp_setpriority01) failed: ENOENT (2) |
| `setpriority02` | 0/7 | fail | TFAIL: setpriority(-1, 0, -2) should fail with EINVAL: ENOSYS (38) |
| `setrlimit01` | 2/4 | partial | TFAIL: setrlimit01.c:183: setrlimit failed, expected 10 got 26 |
| `setrlimit02` | 1/2 | partial | TFAIL: call succeeded unexpectedly |
| `setrlimit03` | 1/2 | partial | TFAIL: call succeeded unexpectedly (nr_open=1048576 rlim_cur=1024 rlim_max=1048577) |
| `setrlimit04` | 1/1 | pass |  |
| `setrlimit05` | 1/1 | pass |  |
| `setrlimit06` | 0/1 | fail | TFAIL: Got no signal after reaching both limit |
