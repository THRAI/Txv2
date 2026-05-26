# LTP process Progress

`process` batch local tracking. Cases are from `tools/ltp-batches.py --batch process`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 109 | from `make ltp-batch-cases LTP_BATCH=process` |
| latest local run | timeout triage | 2026-05-26 single-case reruns for previously hung process cases |
| cumulative scored | `163/355` | recorded rows in this document |
| reached case | `waitpid13` | batch completed |
| logs | `target/oscomp/ltp-progress/process` | per-group stdout and serial snapshots |

## 2026-05-26 failure notes

- TBROK: 25 recorded case(s); see per-case notes below.
- host timeout before case completed: 4 recorded case(s); see per-case notes below.
- TCONF: 15 recorded case(s); see per-case notes below.
- TFAIL: 13 recorded case(s); see per-case notes below.

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
| `clone08` | 3/5 | partial | TBROK: CLONE_PARENT clone() failed: EINVAL (22) |
| `clone09` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/net/ipv4/conf/lo/tag' for reading: ENOENT (2) |
| `clone301` | 0/1 | skip | TCONF: syscall(435) __NR_clone3 not supported on your arch |
| `clone302` | 1/2 | partial | TCONF: syscall(435) __NR_clone3 not supported on your arch |
| `clone303` | 0/1 | fail | TBROK: Can't open /proc/self/mounts: ENOENT (2) |
| `execl01` | 1/1 | pass |  |
| `execle01` | 1/1 | pass |  |
| `execlp01` | 1/1 | pass |  |
| `execv01` | 1/1 | pass |  |
| `execve01` | 1/1 | pass |  |
| `execve02` | 0/1 | fail | TFAIL: execve_child shouldn't be executed |
| `execve03` | 3/6 | partial | TFAIL: execve failed unexpectedly; expected Filename too long: ENOENT (2) |
| `execve04` | 0/1 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `execve05` | 0/9 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `execve06` | 1/1 | pass |  |
| `execveat01` | 0/1 | skip | TCONF: syscall(281) __NR_execveat not supported on your arch |
| `execveat02` | 0/1 | skip | single-case rerun exits cleanly; TCONF: `__NR_execveat` not supported |
| `execveat03` | 0/2 | fail | single-case rerun exits cleanly; test device create/acquire fails with `EINVAL` |
| `execvp01` | 1/1 | pass |  |
| `exit01` | 1/1 | pass |  |
| `exit02` | 1/1 | pass |  |
| `exit_group01` | 1/1 | pass |  |
| `fork01` | 2/2 | pass |  |
| `fork03` | 1/1 | pass |  |
| `fork04` | 1/2 | partial | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
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
| `getpid01` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
| `getpid02` | 2/2 | pass |  |
| `getppid01` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
| `getppid02` | 1/1 | pass |  |
| `getsid01` | 1/1 | pass |  |
| `getsid02` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
| `gettid01` | 0/1 | fail | TBROK: Failed to open FILE '/proc/self/status' for reading: ENOENT (2) |
| `gettid02` | 11/11 | pass |  |
| `kcmp01` | 0/5 | skip | TCONF: syscall(272) __NR_kcmp not supported on your arch |
| `kcmp02` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
| `kcmp03` | 0/0 | hang | TCONF: syscall(272) __NR_kcmp not supported on your arch |
| `personality01` | 0/18 | fail | single-case rerun exits cleanly; all variants fail with `ENOSYS` |
| `personality02` | 0/1 | fail | single-case rerun exits cleanly; `personality()` returns `ENOSYS` |
| `pidfd_getfd01` | 0/1 | skip | single-case rerun exits cleanly; TCONF: `__NR_pidfd_open` not supported |
| `pidfd_getfd02` | 0/1 | skip | single-case rerun exits cleanly; TCONF: `__NR_pidfd_open` not supported |
| `pidfd_open01` | 0/1 | skip | TCONF: syscall(434) __NR_pidfd_open not supported on your arch |
| `pidfd_open02` | 0/1 | skip | TCONF: syscall(434) __NR_pidfd_open not supported on your arch |
| `pidfd_open03` | 0/1 | skip | TCONF: syscall(434) __NR_pidfd_open not supported on your arch |
| `pidfd_open04` | 0/1 | skip | TCONF: syscall(434) __NR_pidfd_open not supported on your arch |
| `pidfd_send_signal01` | 0/1 | skip | TCONF: syscall(424) __NR_pidfd_send_signal not supported on your arch |
| `pidfd_send_signal02` | 0/1 | skip | TCONF: syscall(424) __NR_pidfd_send_signal not supported on your arch |
| `pidfd_send_signal03` | 0/1 | skip | TCONF: syscall(424) __NR_pidfd_send_signal not supported on your arch |
| `process_vm_readv01` | 0/0 | skip |  |
| `process_vm_readv02` | 0/1 | skip | TCONF: syscall(270) __NR_process_vm_readv not supported on your arch |
| `process_vm_readv03` | 0/1 | skip | TCONF: syscall(270) __NR_process_vm_readv not supported on your arch |
| `process_vm_writev01` | 0/0 | skip |  |
| `process_vm_writev02` | 0/1 | skip | TCONF: syscall(271) __NR_process_vm_writev not supported on your arch |
| `set_robust_list01` | 1/2 | partial | TFAIL: set_robust_list01.c:117: set_robust_list: retval = 0 (expected -1), errno = 0 (expected 22) |
| `set_tid_address01` | 1/1 | pass |  |
| `setpgid01` | 1/2 | partial | TFAIL: setpgid01.c:87: test setpgid(19, 1) fail: TEST_ERRNO=ENOSYS(38): Function not implemented |
| `setpgid02` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
| `setpgid03` | 0/1 | fail | TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `setpgrp01` | 1/1 | pass |  |
| `setpgrp02` | 2/2 | pass |  |
| `setsid01` | 2/4 | partial | TFAIL: setsid01.c:155: setpgid failed, errno :38 |
| `vfork01` | 1/1 | pass |  |
| `vfork02` | 0/2 | fail | TBROK: vfork02.c:199: SIGUSR1 signal is not pending in parent |
| `wait01` | 1/1 | pass |  |
| `wait02` | 1/1 | pass |  |
| `wait401` | 0/0 | hang | single-case rerun still host-times out after LTP init output |
| `wait402` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
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
