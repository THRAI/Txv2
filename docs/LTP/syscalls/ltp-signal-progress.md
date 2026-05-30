# LTP signal Progress

`signal` batch local tracking. Cases are from `tools/ltp-batches.py --batch signal`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 51 | from `make ltp-batch-cases LTP_BATCH=signal` |
| latest local run | full-image signal batch | 2026-05-30 direct QEMU run scored `589/615` |
| cumulative scored | `589/615` | latest full-image batch score |
| reached case | `tkill02` | batch completed |
| logs | `target/oscomp/ltp-progress/signal` | per-group stdout and serial snapshots |

## 2026-05-30 full-image batch run

Direct non-Docker QEMU coverage with the full image completed the `signal`
batch and userspace exited cleanly. The run started and completed 47 cases;
serial snapshot: `target/oscomp/os_serial_out_ltp_signal_full_20260530_173124.txt`.
Local judge score: `589/615`.

- Strong passing clusters: `pause01..03`, `rt_sigaction01..03`,
  `rt_sigsuspend01`, `sigaction02`, `sigaltstack01/02`, `signal01..04`,
  `signalfd*`, and `tkill01`.
- Current semantic blockers: `kill05` permission/child-exit behavior,
  `rt_sigprocmask01`/`sigprocmask01` mask membership, `sigaction01`
  `SA_RESETHAND|SA_SIGINFO` preservation, pending-mask behavior in
  `sigpending02`/`sigsuspend01`, and `tgkill`/`tkill` invalid tgid/tid errno
  ordering.
- Unsupported or image-side cases: `rt_sigqueueinfo01` reports unsupported,
  `rt_sigtimedwait01` and `rt_tgsigqueueinfo01` are missing binaries, and
  old `sgetmask`/`ssetmask` remain unsupported/TCONF.
- Verified with:
  `timeout 300s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/testdata --submit target/oscomp/submit --suite ltp-batch:signal`
  and
  `python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata`.

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
| `kill03` | 3/3 | pass | 2026-05-30 full-image batch passes |
| `kill05` | 0/2 | fail | 2026-05-30 full-image batch: kill succeeds unexpectedly and child exits with wrong value |
| `kill06` | 1/1 | pass |  |
| `kill07` | 1/1 | pass |  |
| `kill08` | 1/1 | pass |  |
| `kill09` | 1/1 | pass |  |
| `kill10` | 0/0 | hang | single-case rerun still host-times out after `RUN LTP CASE kill10` |
| `kill11` | 0/0 | hang | single-case rerun prints early TPASS lines, then host-times out before summary |
| `kill12` | 1/1 | pass | single-case rerun passes |
| `kill13` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `pause01` | 1/1 | pass | 2026-05-30 full-image batch passes |
| `pause02` | 1/1 | pass | 2026-05-30 full-image batch passes |
| `pause03` | 1/1 | pass | 2026-05-30 full-image batch passes |
| `rt_sigaction01` | 150/150 | pass | single-case rerun passes |
| `rt_sigaction02` | 150/150 | pass |  |
| `rt_sigaction03` | 150/150 | pass | EINVAL observed |
| `rt_sigprocmask01` | 0/1 | fail | TFAIL: rt_sigprocmask01.c:134: sigismember call failed: TEST_ERRNO=SUCCESS(0): No error information |
| `rt_sigprocmask02` | 2/2 | pass | EINVAL observed |
| `rt_sigqueueinfo01` | 0/1 | skip | 2026-05-30 full-image batch: TCONF `__NR_rt_sigqueueinfo` unsupported |
| `rt_sigsuspend01` | 2/2 | pass | 2026-05-30 full-image batch passes |
| `rt_sigtimedwait01` | 0/0 | missing | single-case rerun exits cleanly; command not found |
| `rt_tgsigqueueinfo01` | 0/0 | missing | single-case rerun exits cleanly; command not found |
| `sgetmask01` | 0/2 | skip | single-case rerun exits cleanly; TCONF: `__NR_ssetmask` not supported |
| `sigaction01` | 3/4 | partial | 2026-05-30 full-image batch: `SA_RESETHAND` still clears `SA_SIGINFO` unexpectedly |
| `sigaction02` | 3/3 | pass | 2026-05-30 full-image batch passes |
| `sigaltstack01` | 1/1 | pass |  |
| `sigaltstack02` | 2/2 | pass | EINVAL observed |
| `sighold02` | 0/1 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `signal01` | 6/6 | pass | 2026-05-30 full-image batch passes |
| `signal02` | 3/3 | pass | 2026-05-30 full-image batch passes |
| `signal03` | 31/31 | pass |  |
| `signal04` | 28/28 | pass |  |
| `signal05` | 30/31 | partial | TFAIL: siglist[n] (18) != sig_pass (17) |
| `signal06` | 0/2 | skip | TCONF: signal06.c:171: Only test on x86_64. |
| `signalfd01` | 2/2 | pass |  |
| `signalfd4_01` | 1/1 | pass |  |
| `signalfd4_02` | 1/1 | pass |  |
| `sigpending02` | 0/1 | fail | 2026-05-30 full-image batch: more than only `SIGUSR1` is pending |
| `sigprocmask01` | 0/1 | fail | TFAIL: sigprocmask01.c:155: sigismember() failed, error:0 |
| `sigrelse01` | 0/2 | fail | TBROK: sigrelse01.c:245: signal() failed for signal 34. error:22 Invalid argument. |
| `sigsuspend01` | 0/1 | fail | 2026-05-30 full-image batch: `sigsuspend()` did not unblock `SIGALRM` |
| `sigtimedwait01` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `sigwait01` | 3/4 | partial | 2026-05-30 full-image batch: expected waits pass, cleanup `kill(..., SIGTERM)` returns `ESRCH` |
| `sigwaitinfo01` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `ssetmask01` | 0/2 | skip | TCONF: ssetmask01.c:115: syscall(-1) __NR_ssetmask not supported on your arch |
| `tgkill01` | 1/2 | partial | 2026-05-30 full-image batch: signal delivery passes, cleanup wait is interrupted |
| `tgkill02` | 0/1 | fail | 2026-05-30 full-image batch: test killed unexpectedly |
| `tgkill03` | 3/6 | partial | 2026-05-30 full-image batch: invalid tgid/tid return `ESRCH` where LTP expects `EINVAL`, then killed by `SIGUSR1` |
| `tkill01` | 2/2 | pass | 2026-05-30 full-image batch passes |
| `tkill02` | 1/2 | partial | 2026-05-30 full-image batch: invalid signal/tid errno ordering still differs |
