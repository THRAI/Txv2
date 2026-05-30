# LTP process Progress

`process` batch local tracking. Cases are from `tools/ltp-batches.py --batch process`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 109 | from `make ltp-batch-cases LTP_BATCH=process` |
| latest local run | focused `pidfd_getfd01+pidfd_getfd02` | 2026-05-30 direct QEMU run scored `6/6` |
| cumulative scored | `318/491` | recorded rows in this document |
| reached case | `fork11` | latest broad run stopped by 300s outer timeout; older per-case rows reach `waitpid13` |
| logs | `target/oscomp/ltp-progress/process` | per-group stdout and serial snapshots |

## 2026-05-30 full-image prefix run

Direct non-Docker QEMU coverage with the full image reached `45/60` before the
300s outer timeout stopped the batch while `fork11` was active. The run started
39 cases and completed 38; serial snapshot:
`target/oscomp/os_serial_out_ltp_process_partial_20260530_171501.txt`.

- Passing prefix clusters: `clone01..03`, `clone05..07`, basic `execl*`,
  `execv01`, `execve01`, `execve05`, `execve06`, `execvp01`, `exit*`, and
  `fork01`, `fork03`, `fork04`, `fork07..10`.
- Main semantic failures: `clone04` user SIGSEGV, `clone08` partial
  `CLONE_PARENT`/clone flag behavior, `execve02`/`execve04` executing the child
  when failure was expected, and `execve03` errno ordering for long paths, bad
  user pointers, and non-executable files.
- Policy/environment blockers: `clone09` still needs
  `/proc/sys/net/ipv4/conf/lo/tag`; `clone3` remains unsupported/TCONF;
  `clone303` now gets past `/proc/self/mounts` and reaches cgroup v2 policy;
  `execveat01/02` still report unsupported arch syscall; `execveat03` breaks
  on test-device acquisition; `fork06` is missing from the image.
- Verified with:
  `timeout 300s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/testdata --submit target/oscomp/submit --suite ltp-batch:process`
  and
  `python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata`.

## 2026-05-30 focused pidfd poll fix

Focused direct QEMU coverage for `pidfd_open03` now passes after pidfds gained
process-identity-lifetime exit readiness. Earlier in the same lane,
`target/oscomp/os_serial_out_ltp_process_tail2_20260530_200725.txt` showed a
kernel panic when `poll(pidfd)` fell through to `OpenFile::rnode()`. The first
fix removed that panic but returned `poll() == 0`; the final fix added a real
pidfd exit wait source and removed the synthetic 5ms recheck timeout.

- Passing evidence: `target/oscomp/os_serial_out_ltp_pidfd_open03_real_wait_20260530_210012.txt`
- Judge: `python3 tools/oscomp-judge.py ...` scored `1/1`.
- Fault decode: no `scause/sepc/stval` trap lines found.

## 2026-05-30 focused pidfd waitid fix

Focused direct QEMU coverage for `pidfd_open04` now passes after the first
`waitid(P_PIDFD)` slice landed. The syscall now accepts pidfd-backed fds for
`P_PIDFD + WEXITED`, returns `EAGAIN` for live nonblocking pidfds, waits on the
pidfd exit source for blocking calls, writes a Linux-shaped `SIGCHLD` siginfo
prefix, and reuses the existing child reap path after target exit. Broader
`waitid` selectors (`P_ALL`, `P_PID`, `P_PGID`), stop/continue reporting, and
full rusage accounting remain deferred.

- Passing evidence: `target/oscomp/os_serial_out_ltp_pidfd_open04_waitid_real_20260530_211111.txt`
- Judge: `python3 tools/oscomp-judge.py ...` scored `3/3`.
- Fault decode: no `scause/sepc/stval` trap lines found.

## 2026-05-30 focused pidfd signal/checkpoint fix

Focused direct QEMU coverage for `pidfd_send_signal01` now passes after futex
wait resume handling stopped treating still-registered wait rows as successful
`FUTEX_WAKE` completions. The signal handler was already receiving the correct
`SA_SIGINFO` payload; the remaining failure was an LTP checkpoint/pthread-join
timeout after a stale signal wake hint made a futex wait return early.

- Passing evidence: `target/oscomp/os_serial_out_ltp_pidfd_send_signal01_futex_retry_20260530_225730.txt`
- Judge: `python3 tools/oscomp-judge.py ...` scored `2/2`.
- Fault decode: no `scause/sepc/stval` trap lines found.

## 2026-05-30 focused pidfd getfd errno fix

Focused direct QEMU coverage for `pidfd_getfd01+pidfd_getfd02` now passes.
The futex wait cleanup fixed the prior checkpoint timeout, leaving one Linux
errno mismatch: `pidfd_getfd(valid_pidfd_to_exited_process, ...)` must return
`ESRCH`, while non-pidfd fds and missing target fds still return `EBADF`.

- Passing evidence: `target/oscomp/os_serial_out_ltp_pidfd_getfd_esrch_20260530_232521.txt`
- Judge: `python3 tools/oscomp-judge.py ...` scored `6/6`.
- Fault decode: no `scause/sepc/stval` trap lines found.

## 2026-05-26 failure notes

- TBROK: 25 recorded case(s); see per-case notes below.
- host timeout before case completed: 4 recorded case(s); see per-case notes below.
- TCONF: 15 recorded case(s); see per-case notes below.
- TFAIL: 11 recorded case(s); see per-case notes below.
- Procfs refresh: `/proc/sys/kernel/pid_max` and `/proc/self/status` are now available; `getpid01`, `getppid01`, `getsid02`, `gettid01`, and `wait402` pass on RV/LA. `kcmp02` now reaches TCONF for missing `kcmp`; `setpgid02` reaches real setpgid errno checks.
- `personality(2)` now records per-process personality state; `personality01` and `personality02` pass on RV/LA.
- Minimal `pidfd_open(2)` fd support is available. `pidfd_open01` and `pidfd_open02` pass on RV/LA. Focused RV reruns now also pass `pidfd_open03` through pidfd poll readiness and `pidfd_open04` through the first `waitid(P_PIDFD)` slice.
- `pidfd_send_signal02` now passes on RV/LA. The pidfd path accepts `pidfd_open` fds and `/proc/<pid>` directory fds, validates flags/siginfo signum, and root `setuid(nonroot)` drops capabilities so the init-process permission case returns `EPERM`.
- `pidfd_send_signal01` now passes in the focused RV run: pidfd signal delivery preserves the expected siginfo payload, and futex checkpoint waits survive stale signal wake hints.
- Minimal `kcmp(2)` support is available. `KCMP_FILE` compares open-file identity and the errno surface is wired; `kcmp01` and `kcmp02` pass on RV/LA. `kcmp03` is still locally skipped.
- Minimal `pidfd_getfd(2)` support is available. It duplicates target-process fds with `FD_CLOEXEC`, returns `ESRCH` for a valid pidfd whose target has exited, and focused RV reruns now pass `pidfd_getfd01` and `pidfd_getfd02`.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `clone01` | 2/2 | pass |  |
| `clone02` | 2/2 | pass |  |
| `clone03` | 1/1 | pass |  |
| `clone04` | 0/1 | fail | TBROK: Test killed by SIGSEGV! |
| `clone05` | 1/1 | pass |  |
| `clone06` | 1/1 | pass |  |
| `clone07` | 1/1 | pass |  |
| `clone08` | 4/5 | partial | 2026-05-30 full-image prefix still partial; previous TBROK: CLONE_PARENT clone() failed: EINVAL (22) |
| `clone09` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/net/ipv4/conf/lo/tag' for reading: ENOENT (2) |
| `clone301` | 0/1 | skip | TCONF: syscall(435) __NR_clone3 not supported on your arch |
| `clone302` | 1/2 | partial | TCONF: syscall(435) __NR_clone3 not supported on your arch |
| `clone303` | 0/1 | skip | 2026-05-30 reaches cgroup policy: V2 base controller TCONF after `/proc/self/mounts` alias fix |
| `execl01` | 1/1 | pass |  |
| `execle01` | 1/1 | pass |  |
| `execlp01` | 1/1 | pass |  |
| `execv01` | 1/1 | pass |  |
| `execve01` | 1/1 | pass |  |
| `execve02` | 0/1 | fail | TFAIL: execve_child shouldn't be executed |
| `execve03` | 3/6 | partial | TFAIL: execve failed unexpectedly; expected Filename too long: ENOENT (2) |
| `execve04` | 0/1 | fail | 2026-05-30 TFAIL: `execve_child` executed when failure was expected |
| `execve05` | 8/8 | pass | 2026-05-30 full-image prefix passes argv/env canary checks |
| `execve06` | 1/1 | pass |  |
| `execveat01` | 0/1 | skip | TCONF: syscall(281) __NR_execveat not supported on your arch |
| `execveat02` | 0/1 | skip | single-case rerun exits cleanly; TCONF: `__NR_execveat` not supported |
| `execveat03` | 0/2 | fail | 2026-05-30 full-image prefix: test device acquisition TBROK |
| `execvp01` | 1/1 | pass |  |
| `exit01` | 1/1 | pass |  |
| `exit02` | 1/1 | pass |  |
| `exit_group01` | 1/1 | pass |  |
| `fork01` | 2/2 | pass |  |
| `fork03` | 1/1 | pass |  |
| `fork04` | 3/3 | pass | 2026-05-30 full-image prefix passes environment inheritance/isolation checks |
| `fork05` | 0/0 | skip |  |
| `fork06` | 0/0 | skip |  |
| `fork07` | 1/1 | pass |  |
| `fork08` | 1/1 | pass |  |
| `fork09` | 1/1 | pass |  |
| `fork10` | 2/2 | pass |  |
| `fork11` | 0/0 | skip |  |
| `fork13` | 0/1 | fail | TBROK: Path not found: /proc/sys/kernel/pid_max: ENOENT (2) |
| `fork14` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `get_robust_list01` | 4/5 | partial | single-case rerun exits cleanly; final lookup fails with `ESRCH` |
| `getpgid01` | 4/8 | partial | single-case rerun exits cleanly; parent/init pgid lookups return `ESRCH` |
| `getpgid02` | 2/2 | pass | single-case rerun passes |
| `getpgrp01` | 2/2 | pass | single-case rerun passes |
| `getpid01` | 100/100 | pass | `/proc/sys/kernel/pid_max` available; RV/LA pass |
| `getpid02` | 2/2 | pass |  |
| `getppid01` | 1/1 | pass | `/proc/sys/kernel/pid_max` available; RV/LA pass |
| `getppid02` | 1/1 | pass |  |
| `getsid01` | 1/1 | pass |  |
| `getsid02` | 1/1 | pass | `/proc/sys/kernel/pid_max` available; unused pid returns ESRCH; RV/LA pass |
| `gettid01` | 2/2 | pass | `/proc/self/status` available; tid matches pid; RV/LA pass |
| `gettid02` | 11/11 | pass |  |
| `kcmp01` | 5/5 | pass | `KCMP_FILE` open-file identity comparisons pass on RV/LA |
| `kcmp02` | 6/6 | pass | bad pid, invalid type, and bad fd errno cases pass on RV/LA |
| `kcmp03` | 0/0 | skip | local skip after `kcmp` support; clone-sharing comparisons still deferred |
| `personality01` | 18/18 | pass | per-process `personality(2)` read/write state; RV/LA pass |
| `personality02` | 1/1 | pass | `STICKY_TIMEOUTS` personality read/write works; `select` keeps timeout unchanged; RV/LA pass |
| `pidfd_getfd01` | 1/1 | pass | 2026-05-30 focused RV run: fd duplication and `kcmp` identity check pass |
| `pidfd_getfd02` | 5/5 | pass | 2026-05-30 focused RV run: invalid pidfd/targetfd/flags, dead-target `ESRCH`, and permission `EPERM` cases pass |
| `pidfd_open01` | 1/1 | pass | pidfd fd installs `FD_CLOEXEC`; RV/LA pass |
| `pidfd_open02` | 3/3 | pass | expired pid, invalid pid, and invalid flags return expected errno; RV/LA pass |
| `pidfd_open03` | 1/1 | pass | 2026-05-30 focused RV run: `poll(pidfd)` wakes on target process exit through identity-lifetime pidfd readiness |
| `pidfd_open04` | 3/3 | pass | 2026-05-30 focused RV run: `PIDFD_NONBLOCK` reflected by `F_GETFL`; `waitid(P_PIDFD)` returns `EAGAIN` while live and succeeds after child exit |
| `pidfd_send_signal01` | 2/2 | pass | 2026-05-30 focused RV run: pidfd signal siginfo delivery and LTP futex checkpoint cleanup pass |
| `pidfd_send_signal02` | 4/4 | pass | pidfd/proc-dir fd errno surface passes on RV/LA |
| `pidfd_send_signal03` | 0/1 | skip | syscall is available; TCONF: `/proc/sys/kernel/ns_last_pid` does not exist |
| `process_vm_readv01` | 0/0 | skip |  |
| `process_vm_readv02` | 0/1 | skip | TCONF: syscall(270) __NR_process_vm_readv not supported on your arch |
| `process_vm_readv03` | 0/1 | skip | TCONF: syscall(270) __NR_process_vm_readv not supported on your arch |
| `process_vm_writev01` | 0/0 | skip |  |
| `process_vm_writev02` | 0/1 | skip | TCONF: syscall(271) __NR_process_vm_writev not supported on your arch |
| `set_robust_list01` | 1/2 | partial | TFAIL: set_robust_list01.c:117: set_robust_list: retval = 0 (expected -1), errno = 0 (expected 22) |
| `set_tid_address01` | 1/1 | pass |  |
| `setpgid01` | 1/2 | partial | TFAIL: setpgid01.c:87: test setpgid(19, 1) fail: TEST_ERRNO=ENOSYS(38): Function not implemented |
| `setpgid02` | 0/3 | fail | `/proc/sys/kernel/pid_max` available; remaining errno mismatches/ENOSYS in setpgid semantics |
| `setpgid03` | 0/1 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `setpgrp01` | 1/1 | pass |  |
| `setpgrp02` | 2/2 | pass |  |
| `setsid01` | 2/4 | partial | TFAIL: setsid01.c:155: setpgid failed, errno :38 |
| `vfork01` | 1/1 | pass |  |
| `vfork02` | 0/2 | fail | TBROK: vfork02.c:199: SIGUSR1 signal is not pending in parent |
| `wait01` | 1/1 | pass |  |
| `wait02` | 1/1 | pass |  |
| `wait401` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `wait402` | 1/1 | pass | `/proc/sys/kernel/pid_max` available; wait4(pid_max + 1) returns ECHILD; RV/LA pass |
| `wait403` | 0/1 | fail | TFAIL: wait4 fails with ESRCH expected ESRCH: EINVAL (22) |
| `waitid01` | 0/6 | fail | TBROK: Invalid child (16) exit value 123 |
| `waitid02` | 0/1 | fail | TFAIL: waitid(P_ALL, 0, infop, WNOHANG) expected EINVAL: ENOSYS (38) |
| `waitid03` | 0/1 | fail | TFAIL: waitid(P_ALL, 0, infop, WNOHANG / WEXITED) expected ECHILD: ENOSYS (38) |
| `waitid04` | 1/4 | partial | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `waitid05` | 1/6 | partial | TFAIL: waitid(P_PGID, pid_group+1, infop, WEXITED) expected ECHILD: ENOSYS (38) |
| `waitid06` | 1/6 | partial | TFAIL: waitid(P_PID, pid_child+1, infop, WEXITED) expected ECHILD: ENOSYS (38) |
| `waitid07` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `waitid08` | 0/0 | hang | prints several `waitid(...)=ENOSYS` TFAIL lines, then host-times out before summary |
| `waitid09` | 0/1 | fail | TFAIL: waitid(P_PID, 1, infop, WEXITED) expected ECHILD: ENOSYS (38) |
| `waitid10` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/core_pattern' for reading: ENOENT (2) |
| `waitid11` | 0/6 | fail | TBROK: Child (17) killed by signal SIGKILL |
| `waitpid01` | 84/115 | partial | TFAIL: WIFSIGNALED() not set in status (exited with 0) |
| `waitpid03` | 2/2 | pass |  |
| `waitpid04` | 2/4 | partial | TFAIL: waipid(-1, NULL, 0xffffffff) expected EINVAL: ECHILD (10) |
| `waitpid06` | 0/9 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `waitpid07` | 0/0 | hang | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `waitpid08` | 0/9 | fail | single-case rerun exits cleanly after checkpoint wait/wake timeout and LTP SIGKILL cleanup |
| `waitpid09` | 0/2 | fail | single-case rerun exits cleanly after checkpoint wait/wake timeout and LTP SIGKILL cleanup |
| `waitpid10` | 0/9 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `waitpid11` | 0/0 | hang | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `waitpid12` | 0/9 | fail | single-case rerun exits cleanly after checkpoint wait/wake timeout and LTP SIGKILL cleanup |
| `waitpid13` | 0/9 | fail | single-case rerun exits cleanly after checkpoint wait/wake timeout and LTP SIGKILL cleanup |
