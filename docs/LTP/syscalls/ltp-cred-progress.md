# LTP cred Progress

`cred` batch local tracking. Cases are from `tools/ltp-batches.py --batch cred`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 131 | from `make ltp-batch-cases LTP_BATCH=cred` |
| latest local run | focused LA64 submit-tail rerun | 2026-06-03 promoted whitelist candidates, musl+glibc |
| cumulative scored | `125/290` | recorded rows in this document |
| reached case | `setuid04_16` | batch completed |
| logs | `target/oscomp/ltp-progress/cred` | per-group stdout and serial snapshots |

## 2026-06-03 focused submit-tail rerun

复测日志：

- LA musl: `target/oscomp/ltp-extra-core-a-la-musl-20260603.txt`
- LA glibc: `target/oscomp/ltp-extra-core-g1-la-glibc-20260603.txt`

确认可作为 active submit 尾部补充分的 cred case：

`capset04`, `getegid02`, `getegid02_16`, `geteuid01`, `getgid01`,
`getgid03`, `getuid01`, `setgid01`, `setuid01`。

这些 case 在 LA musl/glibc focused run 中均为 Summary 满分。

## 2026-05-26 failure notes

- TCONF: 67 recorded case(s); see per-case notes below.
- TFAIL: 24 recorded case(s); see per-case notes below.
- TBROK: 10 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `add_key01` | 0/1 | skip | TCONF: syscall(217) __NR_add_key not supported on your arch |
| `add_key02` | 0/1 | skip | TCONF: syscall(217) __NR_add_key not supported on your arch |
| `add_key03` | 0/1 | skip | TCONF: syscall(217) __NR_add_key not supported on your arch |
| `add_key04` | 0/1 | skip | TCONF: syscall(219) __NR_keyctl not supported on your arch |
| `add_key05` | 0/1 | skip | TCONF: Couldn't find 'groupdel' in $PATH |
| `capget01` | 6/6 | pass |  |
| `capget02` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
| `capset01` | 3/3 | pass |  |
| `capset02` | 0/1 | fail | TBROK: capset data failed: EPERM (1) |
| `capset03` | 0/1 | fail | TBROK: capset data failed: EPERM (1) |
| `capset04` | 1/1 | pass |  |
| `getegid01` | 0/1 | fail | TBROK: Failed to open FILE '/proc/self/status' for reading: ENOENT (2) |
| `getegid01_16` | 0/1 | fail | TBROK: Failed to open FILE '/proc/self/status' for reading: ENOENT (2) |
| `getegid02` | 1/1 | pass |  |
| `getegid02_16` | 1/1 | pass |  |
| `geteuid01` | 1/1 | pass |  |
| `geteuid01_16` | 0/1 | skip | TCONF: 16-bit version of geteuid() is not supported on your platform |
| `geteuid02` | 1/2 | partial | TBROK: Failed to open FILE '/proc/self/status' for reading: ENOENT (2) |
| `geteuid02_16` | 0/1 | skip | TCONF: 16-bit version of geteuid() is not supported on your platform |
| `getgid01` | 1/1 | pass |  |
| `getgid01_16` | 0/1 | skip | TCONF: 16-bit version of getgid() is not supported on your platform |
| `getgid03` | 1/1 | pass |  |
| `getgid03_16` | 0/1 | skip | TCONF: 16-bit version of getgid() is not supported on your platform |
| `getgroups01` | 0/4 | fail | TFAIL: getgroups01.c:97: getgroups didn't fail as expected with EINVAL: TEST_ERRNO=ENOSYS(38): Function not implemented |
| `getgroups01_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/getgroups/../utils/compat_16.h:82: 16-bit version of getgroups() is not supported on your platform |
| `getgroups03` | 0/1 | fail | TFAIL: getgroups03.c:79: getgroups failed: TEST_ERRNO=ENOSYS(38): Function not implemented |
| `getgroups03_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/getgroups/../utils/compat_16.h:77: 16-bit version of setgroups() is not supported on your platform |
| `getresgid01` | 1/1 | pass |  |
| `getresgid01_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/getresgid/../utils/compat_16.h:151: 16-bit version of getresgid() is not supported on your platform |
| `getresgid02` | 1/1 | pass |  |
| `getresgid02_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/getresgid/../utils/compat_16.h:151: 16-bit version of getresgid() is not supported on your platform |
| `getresgid03` | 1/1 | pass |  |
| `getresgid03_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/getresgid/../utils/compat_16.h:151: 16-bit version of getresgid() is not supported on your platform |
| `getresuid01` | 1/1 | pass |  |
| `getresuid01_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/getresuid/../utils/compat_16.h:141: 16-bit version of getresuid() is not supported on your platform |
| `getresuid02` | 1/1 | pass |  |
| `getresuid02_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/getresuid/../utils/compat_16.h:141: 16-bit version of getresuid() is not supported on your platform |
| `getresuid03` | 1/1 | pass |  |
| `getresuid03_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/getresuid/../utils/compat_16.h:141: 16-bit version of getresuid() is not supported on your platform |
| `getuid01` | 1/1 | pass |  |
| `getuid01_16` | 0/1 | skip | TCONF: 16-bit version of getuid() is not supported on your platform |
| `getuid03` | 1/2 | partial | TBROK: Failed to open FILE '/proc/self/status' for reading: ENOENT (2) |
| `getuid03_16` | 0/1 | skip | TCONF: 16-bit version of getuid() is not supported on your platform |
| `keyctl01` | 0/1 | skip | TCONF: syscall(219) __NR_keyctl not supported on your arch |
| `keyctl02` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/keys/root_maxkeys' for reading: ENOENT (2) |
| `keyctl03` | 0/1 | skip | TCONF: syscall(217) __NR_add_key not supported on your arch |
| `keyctl04` | 0/1 | skip | TCONF: syscall(219) __NR_keyctl not supported on your arch |
| `keyctl05` | 0/1 | fail | TBROK: 'modprobe' exited with a non-zero code 1 at tst_cmd.c:121 |
| `keyctl06` | 0/1 | skip | TCONF: syscall(217) __NR_add_key not supported on your arch |
| `keyctl07` | 0/2 | skip | TCONF: syscall(218) __NR_request_key not supported on your arch |
| `keyctl08` | 0/1 | skip | TCONF: syscall(219) __NR_keyctl not supported on your arch |
| `keyctl09` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `request_key01` | 0/1 | skip | TCONF: syscall(217) __NR_add_key not supported on your arch |
| `request_key02` | 0/1 | skip | TCONF: syscall(217) __NR_add_key not supported on your arch |
| `request_key03` | 0/1 | skip | TCONF: syscall(219) __NR_keyctl not supported on your arch |
| `request_key04` | 0/1 | skip | TCONF: syscall(219) __NR_keyctl not supported on your arch |
| `request_key05` | 0/1 | skip | TCONF: syscall(218) __NR_request_key not supported on your arch |
| `setegid01` | 4/4 | pass |  |
| `setegid02` | 0/1 | fail | TFAIL: setegid(65534) succeeded |
| `setfsgid01` | 0/3 | fail | TFAIL: SETFSGID(nobody_gid) retval -1 != 0: ENOSYS (38) |
| `setfsgid01_16` | 0/1 | skip | TCONF: 16-bit version of setfsgid() is not supported on your platform |
| `setfsgid02` | 0/4 | fail | TFAIL: EUID 65534: setfsgid() returned -1 |
| `setfsgid02_16` | 0/1 | skip | TCONF: 16-bit version of setfsgid() is not supported on your platform |
| `setfsgid03` | 0/1 | fail | TFAIL: setfsgid03.c:67: setfsgid() failed unexpectedly: TEST_ERRNO=ENOSYS(38): Function not implemented |
| `setfsgid03_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/setfsgid/../utils/compat_16.h:122: 16-bit version of setfsgid() is not supported on your platform |
| `setfsuid01` | 0/2 | fail | TFAIL: setfsuid(65534) retval -1 != 0: ENOSYS (38) |
| `setfsuid01_16` | 0/1 | skip | TCONF: 16-bit version of setfsuid() is not supported on your platform |
| `setfsuid02` | 0/2 | fail | TFAIL: SETFSUID(invalid_uid) retval -1 != 0: ENOSYS (38) |
| `setfsuid02_16` | 0/1 | skip | TCONF: 16-bit version of setfsuid() is not supported on your platform |
| `setfsuid03` | 0/2 | fail | TFAIL: SETFSUID(ruid) retval -1 != 65534: ENOSYS (38) |
| `setfsuid03_16` | 0/1 | skip | TCONF: 16-bit version of setfsuid() is not supported on your platform |
| `setfsuid04` | 0/1 | fail | TFAIL:  |
| `setfsuid04_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/setfsuid/../utils/compat_16.h:117: 16-bit version of setfsuid() is not supported on your platform |
| `setgid01` | 1/1 | pass |  |
| `setgid01_16` | 0/1 | skip | TCONF: 16-bit version of setgid() is not supported on your platform |
| `setgid02` | 0/1 | fail | TFAIL: SETGID(rootpwent->pw_gid) succeeded |
| `setgid02_16` | 0/1 | skip | TCONF: 16-bit version of setgid() is not supported on your platform |
| `setgid03` | 2/2 | pass |  |
| `setgid03_16` | 0/1 | skip | TCONF: 16-bit version of setgid() is not supported on your platform |
| `setgroups01` | 0/1 | fail | TBROK: getgroups() Failed: ENOSYS (38) |
| `setgroups01_16` | 0/1 | skip | TCONF: 16-bit version of getgroups() is not supported on your platform |
| `setgroups02` | 1/3 | partial | TFAIL: GETGROUPS(1, groups_get) retval -1 != 1: ENOSYS (38) |
| `setgroups02_16` | 0/1 | skip | TCONF: 16-bit version of setgroups() is not supported on your platform |
| `setgroups03` | 1/3 | partial | TFAIL: setgroups(33, groups_list) succeeded |
| `setgroups03_16` | 0/1 | skip | TCONF: 16-bit version of setgroups() is not supported on your platform |
| `setregid01` | 5/5 | pass |  |
| `setregid01_16` | 0/1 | skip | TCONF: 16-bit version of setregid() is not supported on your platform |
| `setregid02` | 0/12 | fail | TFAIL: setregid(-1, 0) did not fail (ret: 0) as expected (ret: -1). |
| `setregid02_16` | 0/1 | skip | TCONF: 16-bit version of setregid() is not supported on your platform |
| `setregid03` | 16/22 | partial | TFAIL: setregid(1, -1) succeeded unexpectedly |
| `setregid03_16` | 0/1 | skip | TCONF: 16-bit version of setregid() is not supported on your platform |
| `setregid04` | 9/9 | pass |  |
| `setregid04_16` | 0/1 | skip | TCONF: 16-bit version of setregid() is not supported on your platform |
| `setresgid01` | 5/5 | pass |  |
| `setresgid01_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/setresgid/../utils/compat_16.h:146: 16-bit version of setresgid() is not supported on your platform |
| `setresgid02` | 6/6 | pass |  |
| `setresgid02_16` | 0/1 | skip | TCONF: 16-bit version of setresgid() is not supported on your platform |
| `setresgid03` | 0/4 | fail | TFAIL: setresgid(-1, -1, other) succeeded |
| `setresgid03_16` | 0/1 | skip | TCONF: 16-bit version of setresgid() is not supported on your platform |
| `setresgid04` | 1/1 | pass |  |
| `setresgid04_16` | 0/2 | skip | TCONF: /code/ltp-full-20240524/testcases/kernel/syscalls/setresgid/../utils/compat_16.h:146: 16-bit version of setresgid() is not supported on your platform |
| `setresuid01` | 9/9 | pass |  |
| `setresuid01_16` | 0/1 | skip | TCONF: 16-bit version of setresuid() is not supported on your platform |
| `setresuid02` | 4/4 | pass |  |
| `setresuid02_16` | 0/1 | skip | TCONF: 16-bit version of setresuid() is not supported on your platform |
| `setresuid03` | 0/3 | fail | TFAIL: setresuid(other, -1, -1) succeeded |
| `setresuid03_16` | 0/1 | skip | TCONF: 16-bit version of setresuid() is not supported on your platform |
| `setresuid04` | 1/3 | partial | TFAIL: open(TEMP_FILE, O_RDWR) succeeded |
| `setresuid04_16` | 0/1 | skip | TCONF: 16-bit version of setresuid() is not supported on your platform |
| `setresuid05` | 2/2 | pass |  |
| `setresuid05_16` | 0/1 | skip | TCONF: 16-bit version of setresuid() is not supported on your platform |
| `setreuid01` | 7/7 | pass |  |
| `setreuid01_16` | 0/1 | skip | TCONF: 16-bit version of setreuid() is not supported on your platform |
| `setreuid02` | 7/7 | pass |  |
| `setreuid02_16` | 0/1 | skip | TCONF: 16-bit version of setreuid() is not supported on your platform |
| `setreuid03` | 4/14 | partial | TFAIL: setreuid(-1, root) succeeded |
| `setreuid03_16` | 0/1 | skip | TCONF: 16-bit version of setreuid() is not supported on your platform |
| `setreuid04` | 3/3 | pass |  |
| `setreuid04_16` | 0/1 | skip | TCONF: 16-bit version of setreuid() is not supported on your platform |
| `setreuid05` | 11/15 | partial | TFAIL: setreuid(-1, root) succeeded |
| `setreuid05_16` | 0/1 | skip | TCONF: 16-bit version of setreuid() is not supported on your platform |
| `setreuid06` | 0/3 | fail | TFAIL: setreuid(-1, 1) succeeded |
| `setreuid06_16` | 0/1 | skip | TCONF: 16-bit version of setreuid() is not supported on your platform |
| `setreuid07` | 1/3 | partial | TFAIL: open(TEMPFILE, O_RDWR) succeeded |
| `setreuid07_16` | 0/1 | skip | TCONF: 16-bit version of setreuid() is not supported on your platform |
| `setuid01` | 1/1 | pass |  |
| `setuid01_16` | 0/1 | skip | TCONF: 16-bit version of setuid() is not supported on your platform |
| `setuid03` | 0/1 | fail | TFAIL: SETUID(ROOT_USER) succeeded |
| `setuid03_16` | 0/1 | skip | TCONF: 16-bit version of setuid() is not supported on your platform |
| `setuid04` | 0/2 | fail | TFAIL: open() succeeded unexpectedly |
| `setuid04_16` | 0/1 | skip | TCONF: 16-bit version of setuid() is not supported on your platform |
