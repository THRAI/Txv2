# LTP signal Progress

`signal` batch local tracking. Cases are from `tools/ltp-batches.py --batch signal`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 51 | from `make ltp-batch-cases LTP_BATCH=signal` |
| latest local run | timeout triage | 2026-05-26 single-case reruns for previously hung signal cases |
| cumulative scored | `561/600` | recorded rows in this document |
| reached case | `tkill02` | batch completed |
| logs | `target/oscomp/ltp-progress/signal` | per-group stdout and serial snapshots |

## 2026-05-26 failure notes

- host timeout before case completed: 9 recorded case(s); see per-case notes below.
- TBROK: 9 recorded case(s); see per-case notes below.
- TFAIL: 6 recorded case(s); see per-case notes below.
- EINVAL observed: 3 recorded case(s); see per-case notes below.
- TCONF: 3 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `kill02` | 2/2 | pass |  |
| `kill03` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
| `kill05` | 0/2 | fail | TBROK: Invalid child (19) exit value 1 |
| `kill06` | 1/1 | pass |  |
| `kill07` | 1/1 | pass |  |
| `kill08` | 1/1 | pass |  |
| `kill09` | 1/1 | pass |  |
| `kill10` | 0/0 | hang | single-case rerun still host-times out after `RUN LTP CASE kill10` |
| `kill11` | 0/0 | hang | single-case rerun prints early TPASS lines, then host-times out before summary |
| `kill12` | 1/1 | pass | single-case rerun passes |
| `kill13` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `pause01` | 0/1 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `pause02` | 0/0 | hang | single-case rerun still host-times out after `RUN LTP CASE pause02` |
| `pause03` | 0/0 | hang | single-case rerun still host-times out after `RUN LTP CASE pause03` |
| `rt_sigaction01` | 150/150 | pass | single-case rerun passes |
| `rt_sigaction02` | 150/150 | pass |  |
| `rt_sigaction03` | 150/150 | pass | EINVAL observed |
| `rt_sigprocmask01` | 0/1 | fail | TFAIL: rt_sigprocmask01.c:134: sigismember call failed: TEST_ERRNO=SUCCESS(0): No error information |
| `rt_sigprocmask02` | 2/2 | pass | EINVAL observed |
| `rt_sigqueueinfo01` | 0/1 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `rt_sigsuspend01` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `rt_sigtimedwait01` | 0/0 | missing | single-case rerun exits cleanly; command not found |
| `rt_tgsigqueueinfo01` | 0/0 | missing | single-case rerun exits cleanly; command not found |
| `sgetmask01` | 0/2 | skip | single-case rerun exits cleanly; TCONF: `__NR_ssetmask` not supported |
| `sigaction01` | 1/2 | partial | single-case rerun exits cleanly; `SA_RESETHAND` clears `SA_SIGINFO` unexpectedly |
| `sigaction02` | 1/5 | partial | TFAIL: sigaction02.c:125: sigaction() succeeded, should have failed |
| `sigaltstack01` | 1/1 | pass |  |
| `sigaltstack02` | 2/2 | pass | EINVAL observed |
| `sighold02` | 0/1 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `signal01` | 0/0 | hang | TFAIL: (long)signal(SIGKILL, tc->sighandler) succeeded |
| `signal02` | 1/3 | partial | TFAIL: (long)signal(sigs[n], SIG_IGN) succeeded |
| `signal03` | 31/31 | pass |  |
| `signal04` | 28/28 | pass |  |
| `signal05` | 30/31 | partial | TFAIL: siglist[n] (18) != sig_pass (17) |
| `signal06` | 0/2 | skip | TCONF: signal06.c:171: Only test on x86_64. |
| `signalfd01` | 2/2 | pass |  |
| `signalfd4_01` | 1/1 | pass |  |
| `signalfd4_02` | 1/1 | pass |  |
| `sigpending02` | 0/1 | fail | TBROK: raising SIGUSR1 failed |
| `sigprocmask01` | 0/1 | fail | TFAIL: sigprocmask01.c:155: sigismember() failed, error:0 |
| `sigrelse01` | 0/2 | fail | TBROK: sigrelse01.c:245: signal() failed for signal 34. error:22 Invalid argument. |
| `sigsuspend01` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `sigtimedwait01` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `sigwait01` | 3/4 | partial | single-case rerun exits cleanly; expected waits pass, cleanup `kill(..., SIGTERM)` returns `ESRCH` |
| `sigwaitinfo01` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `ssetmask01` | 0/2 | skip | TCONF: ssetmask01.c:115: syscall(-1) __NR_ssetmask not supported on your arch |
| `tgkill01` | 0/4 | fail | single-case rerun exits cleanly after checkpoint wait/wake timeouts and LTP SIGKILL cleanup |
| `tgkill02` | 0/1 | fail | single-case rerun exits cleanly; `setrlimit()` setup reports TBROK |
| `tgkill03` | 0/4 | fail | single-case rerun exits cleanly after checkpoint wait/wake timeouts and LTP SIGKILL cleanup |
| `tkill01` | 0/2 | fail | single-case rerun exits cleanly; `tkill` returns `ESRCH` and signal is not captured |
| `tkill02` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
