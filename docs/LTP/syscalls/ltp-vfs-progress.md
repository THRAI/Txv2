# LTP vfs Progress

`vfs` batch local tracking. Cases are from `tools/ltp-batches.py --batch vfs`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 258 | from `make ltp-batch-cases LTP_BATCH=vfs` |
| latest local run | focused high-score rerun | 2026-06-03 `open11` RV/LA musl+glibc |
| cumulative scored | `1028/1451` | recorded rows in this document |
| reached case | `utimes01` | batch completed |
| logs | `target/oscomp/ltp-progress/vfs` | per-group stdout and serial snapshots |

## 2026-06-03 focused submit-tail rerun

复测日志：

- LA musl: `target/oscomp/ltp-extra-core-b1-la-musl-20260603.txt` and
  `target/oscomp/ltp-extra-core-b2-la-musl-20260603.txt`
- LA glibc: `target/oscomp/ltp-extra-core-g4-la-glibc-20260603.txt`

确认可作为 active submit 尾部补充分的 VFS case：

`chmod07`, `chown01`, `creat03`, `creat05`, `fchdir01`, `fchdir02`,
`fchmod02`, `fchmod03`, `fchmod04`, `fchmod05`, `flock03`, `getcwd03`,
`mkdir05`, `open03`, `open04`, `readdir01`, `rmdir01`, `symlink02`,
`umask01`。

这些 case 在 LA musl/glibc focused run 中均为 Summary 满分。此前 musl
单跑时 `creat05`/`open04` 可能出现 tmpdir cleanup `TWARN`，但这次 glibc
focused run 没有复现，judge 计分为 `21/21`。

## 2026-06-03 focused high-score rerun

`open11` 复测结果：

- LA musl: `28/28`
- LA glibc: `28/28`
- RV musl: `28/28`
- RV glibc: `28/28`

修正点是目录目标的写打开和 `O_CREAT` 打开已有目录都按 Linux 语义返回
`EISDIR`。对应日志：
`target/oscomp/ltp-highfix-open11-semop02-la-musl-20260603.txt`,
`target/oscomp/ltp-highfix-open11-la-musl-20260603.txt`,
`target/oscomp/ltp-highfix-open11-semop02-la-glibc-20260603.txt`,
`target/oscomp/ltp-highfix-open11-semop02-rv-musl-20260603.txt`, and
`target/oscomp/ltp-highfix-open11-semop02-rv-glibc-20260603.txt`.

## 2026-05-26 failure notes

- TBROK: 49 recorded case(s); see per-case notes below.
- TFAIL: 25 recorded case(s); see per-case notes below.
- test device image creation/acquire fails: 19 recorded case(s); see per-case notes below.
- setup hits `mkdir(mntpoint/dir/) = EEXIST`: 13 recorded case(s); see per-case notes below.
- 16-bit compat syscall unsupported (`TCONF`): 9 recorded case(s); see per-case notes below.
- xattr unsupported/mismatch: 9 recorded case(s); see per-case notes below.
- 2026-05-25 VFS run: 8 recorded case(s); see per-case notes below.
- TCONF: 7 recorded case(s); see per-case notes below.
- host timeout before case completed: 1 recorded case(s); see per-case notes below.
- setup `fsetxattr` returns `ENOSYS`: 5 recorded case(s); see per-case notes below.
- EINVAL observed: 3 recorded case(s); see per-case notes below.
- `fchown` returns `ENOSYS`: 3 recorded case(s); see per-case notes below.
- bad user pointer returns `ENAMETOOLONG`, expected `EFAULT`: 3 recorded case(s); see per-case notes below.
- bad-address/permission errno mismatches; ELOOP path cleanup warning: 2 recorded case(s); see per-case notes below.
- incorrect group ownership: 2 recorded case(s); see per-case notes below.
- mode bits mismatch after chown: 2 recorded case(s); see per-case notes below.
- one invalid truncate case unexpectedly succeeds: 2 recorded case(s); see per-case notes below.
- requires `CONFIG_MANDATORY_FILE_LOCKING=y`: 2 recorded case(s); see per-case notes below.
- setgid directory inheritance/group ownership mismatch: 2 recorded case(s); see per-case notes below.
- symlink lstat reports incorrect values: 2 recorded case(s); see per-case notes below.
- symlink01 alias: 2 recorded case(s); see per-case notes below.
- 1000 hard links leave stat link count at `1`: 1 recorded case(s); see per-case notes below.
- 2026-05-25 VFS run; cross-FS hard link returns `EXDEV`: 1 recorded case(s); see per-case notes below.
- 2026-05-25 VFS run; errno coverage: 1 recorded case(s); see per-case notes below.
- 2026-05-25 VFS run; local txv2 file handle: 1 recorded case(s); see per-case notes below.
- TWARN: 1 recorded case(s); see per-case notes below.
- `<sys/xattr.h>` or `<sys/acl.h>` missing: 1 recorded case(s); see per-case notes below.
- `AT_SYMLINK_NOFOLLOW` symlink ownership not changed as expected: 1 recorded case(s); see per-case notes below.
- `__NR_futimesat` unsupported on this arch: 1 recorded case(s); see per-case notes below.
- `fchdir()` unexpectedly succeeds: 1 recorded case(s); see per-case notes below.
- `fchown` returns `ENOSYS`; mode check also mismatches: 1 recorded case(s); see per-case notes below.
- `fchown` returns `ENOSYS`; ownership stat checks fail: 1 recorded case(s); see per-case notes below.
- `getdents64` finds files but misses `.` and `..`; old `getdents` unsupported: 1 recorded case(s); see per-case notes below.
- `tst_checkpoint_wait` timed out: 1 recorded case(s); see per-case notes below.
- absolute path with invalid dirfd returns `EBADF`; expected success: 1 recorded case(s); see per-case notes below.
- bad address errno and directory permission checks mismatch: 1 recorded case(s); see per-case notes below.
- executable fixture lookup fails as `sh: 1 recorded case(s); see per-case notes below.
- expected failure cases unexpectedly succeeded: 1 recorded case(s); see per-case notes below.
- fixed shared/exclusive flock compatibility and release on close: 1 recorded case(s); see per-case notes below.
- focused `fstatat/statx` run: 1 recorded case(s); see per-case notes below.
- `tst_checkpoint_*` timeout in single-case run: 1 recorded case(s); see per-case notes below.
- hard link succeeds but link count remains `1`: 1 recorded case(s); see per-case notes below.
- invalid address/flags/mode and `AT_EACCESS` checks mismatch: 1 recorded case(s); see per-case notes below.
- long/bad path errno mismatch: 1 recorded case(s); see per-case notes below.
- needs at least 2 CPUs online: 1 recorded case(s); see per-case notes below.
- nested mkdir unexpectedly succeeds: 1 recorded case(s); see per-case notes below.
- old `getdents` unsupported; `getdents64`/libc variants pass: 1 recorded case(s); see per-case notes below.
- permission, bad-address, and search-permission errno mismatches: 1 recorded case(s); see per-case notes below.
- regular-file chmod reports success but stat mode remains `0644`; directory mode passes: 1 recorded case(s); see per-case notes below.
- setup `realpath()` fails with `ENOENT`: 1 recorded case(s); see per-case notes below.
- setup `setxattr` returns `ENOSYS`: 1 recorded case(s); see per-case notes below.
- size/invalid-buffer errno mismatch: 1 recorded case(s); see per-case notes below.
- sticky bit on regular file not reflected in stat mode: 1 recorded case(s); see per-case notes below.
- symlink/lstat setup breaks in symlink01 alias: 1 recorded case(s); see per-case notes below.
- symlink01 alias/lstat setup breaks: 1 recorded case(s); see per-case notes below.
- symlink01 alias; runner maps to `symlink01 -T chdir01`: 1 recorded case(s); see per-case notes below.
- symlink01 alias; runner maps to `symlink01 -T chmod01`: 1 recorded case(s); see per-case notes below.
- uid/gid changes do not appear in later stat checks: 1 recorded case(s); see per-case notes below.
- unprivileged `O_NOATIME` unexpectedly succeeds: 1 recorded case(s); see per-case notes below.
- unprivileged permission checks too permissive: 1 recorded case(s); see per-case notes below.
- verified with flock regression run: 1 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `access01` | 147/199 | partial | unprivileged permission checks too permissive |
| `access02` | 12/16 | partial | executable fixture lookup fails as `sh: ./file_x: not found` |
| `access03` | 0/8 | fail | bad user pointer returns `ENAMETOOLONG`, expected `EFAULT` |
| `access04` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `chdir01` | 0/2 | fail | test device image creation/acquire fails |
| `chdir01A` | 0/3 | fail | symlink01 alias; runner maps to `symlink01 -T chdir01` |
| `chdir04` | 1/3 | partial | long/bad path errno mismatch: `ENOENT`/`ENAMETOOLONG` vs expected |
| `chmod01` | 24/32 | partial | regular-file chmod reports success but stat mode remains `0644`; directory mode passes |
| `chmod01A` | 0/3 | fail | symlink01 alias; runner maps to `symlink01 -T chmod01` |
| `chmod03` | 3/4 | partial | sticky bit on regular file not reflected in stat mode |
| `chmod05` | 1/1 | pass |  |
| `chmod06` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `chmod07` | 1/1 | pass |  |
| `chown01` | 1/1 | pass |  |
| `chown01_16` | 0/1 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `chown02` | 2/3 | partial | mode bits mismatch after chown |
| `chown02_16` | 0/1 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `chown03` | 1/2 | partial | mode bits mismatch after chown |
| `chown03_16` | 0/1 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `chown04` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `chown04_16` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `chown05` | 6/12 | partial | uid/gid changes do not appear in later stat checks |
| `chown05_16` | 0/1 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `creat01` | 6/6 | pass |  |
| `creat03` | 1/1 | pass |  |
| `creat04` | 0/2 | fail | expected failure cases unexpectedly succeeded |
| `creat05` | 1/1 | pass |  |
| `creat06` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `creat07` | 0/1 | fail | `tst_checkpoint_wait` timed out |
| `creat08` | 6/9 | partial | setgid directory inheritance/group ownership mismatch |
| `creat09` | 0/2 | fail | test device image creation/acquire fails |
| `faccessat01` | 3/3 | pass |  |
| `faccessat02` | 2/2 | pass |  |
| `faccessat201` | 5/7 | partial | absolute path with invalid dirfd returns `EBADF`; expected success |
| `faccessat202` | 2/6 | partial | invalid address/flags/mode and `AT_EACCESS` checks mismatch |
| `fchdir01` | 1/1 | pass |  |
| `fchdir02` | 1/1 | pass |  |
| `fchdir03` | 0/1 | fail | `fchdir()` unexpectedly succeeds |
| `fchmod01` | 8/8 | pass | 2026-05-25 VFS run |
| `fchmod02` | 1/1 | pass | 2026-05-25 VFS run |
| `fchmod03` | 1/1 | pass | 2026-05-25 VFS run |
| `fchmod04` | 1/1 | pass | 2026-05-25 VFS run |
| `fchmod05` | 1/1 | pass | 2026-05-25 VFS run |
| `fchmod06` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `fchmodat01` | 6/6 | pass |  |
| `fchmodat02` | 5/6 | partial | bad user pointer returns `ENAMETOOLONG`, expected `EFAULT` |
| `fchown01` | 0/1 | fail | `fchown` returns `ENOSYS` |
| `fchown01_16` | 0/1 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `fchown02` | 0/3 | fail | `fchown` returns `ENOSYS`; mode check also mismatches |
| `fchown02_16` | 0/1 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `fchown03` | 0/1 | fail | `fchown` returns `ENOSYS` |
| `fchown03_16` | 0/1 | fail | `fchown` returns `ENOSYS` |
| `fchown04` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `fchown04_16` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `fchown05` | 0/12 | fail | `fchown` returns `ENOSYS`; ownership stat checks fail |
| `fchown05_16` | 0/1 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `fchownat01` | 5/5 | pass |  |
| `fchownat02` | 0/1 | fail | `AT_SYMLINK_NOFOLLOW` symlink ownership not changed as expected |
| `fgetxattr01` | 0/2 | fail | test device image creation/acquire fails |
| `fgetxattr02` | 0/1 | fail | setup `fsetxattr` returns `ENOSYS` |
| `fgetxattr03` | 0/1 | fail | setup `fsetxattr` returns `ENOSYS` |
| `flistxattr01` | 0/1 | fail | setup `fsetxattr` returns `ENOSYS` |
| `flistxattr02` | 0/1 | fail | setup `fsetxattr` returns `ENOSYS` |
| `flistxattr03` | 0/1 | fail | setup `fsetxattr` returns `ENOSYS` |
| `flock01` | 3/3 | pass |  |
| `flock02` | 3/3 | pass |  |
| `flock03` | 1/3 | partial | single-case run confirms initial flock passes, then `tst_checkpoint_wait/wake` time out |
| `flock04` | 6/6 | pass | fixed shared/exclusive flock compatibility and release on close |
| `flock06` | 4/4 | pass | verified with flock regression run |
| `fremovexattr01` | 0/2 | fail | test device image creation/acquire fails |
| `fremovexattr02` | 0/2 | fail | test device image creation/acquire fails |
| `fsetxattr01` | 0/2 | fail | test device image creation/acquire fails |
| `fsetxattr02` | 0/3 | fail | xattr unsupported/mismatch |
| `fstat02` | 6/6 | pass |  |
| `fstat02_64` | 6/6 | pass |  |
| `fstat03` | 2/2 | pass |  |
| `fstat03_64` | 2/2 | pass |  |
| `fstatat01` | 6/6 | pass | focused `fstatat/statx` run |
| `fstatfs01` | 0/2 | fail | test device image creation/acquire fails |
| `fstatfs01_64` | 0/2 | fail | test device image creation/acquire fails |
| `fstatfs02` | 2/2 | pass |  |
| `fstatfs02_64` | 2/2 | pass |  |
| `ftruncate01` | 2/2 | pass |  |
| `ftruncate01_64` | 2/2 | pass |  |
| `ftruncate03` | 3/4 | partial | one invalid truncate case unexpectedly succeeds |
| `ftruncate03_64` | 3/4 | partial | one invalid truncate case unexpectedly succeeds |
| `ftruncate04` | 0/1 | fail | requires `CONFIG_MANDATORY_FILE_LOCKING=y` |
| `ftruncate04_64` | 0/1 | fail | requires `CONFIG_MANDATORY_FILE_LOCKING=y` |
| `futimesat01` | 0/2 | fail | `__NR_futimesat` unsupported on this arch |
| `getcwd01` | 3/5 | partial | size/invalid-buffer errno mismatch |
| `getcwd02` | 0/1 | fail | setup `realpath()` fails with `ENOENT` |
| `getcwd03` | 1/1 | pass |  |
| `getcwd04` | 0/1 | fail | needs at least 2 CPUs online |
| `getdents01` | 0/4 | fail | `getdents64` finds files but misses `.` and `..`; old `getdents` unsupported |
| `getdents02` | 12/13 | partial | old `getdents` unsupported; `getdents64`/libc variants pass |
| `getxattr01` | 0/1 | fail | setup `setxattr` returns `ENOSYS` |
| `getxattr02` | 0/2 | fail | test device image creation/acquire fails |
| `getxattr03` | 0/2 | fail | test device image creation/acquire fails |
| `getxattr04` | 0/2 | fail | test device image creation/acquire fails |
| `getxattr05` | 0/1 | fail | `<sys/xattr.h>` or `<sys/acl.h>` missing |
| `lchown01` | 6/6 | pass | 2026-05-25 VFS run |
| `lchown01_16` | 0/2 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `lchown02` | 3/6 | partial | permission, bad-address, and search-permission errno mismatches |
| `lchown02_16` | 0/2 | fail | 16-bit compat syscall unsupported (`TCONF`) |
| `lchown03` | 0/3 | fail | test device image creation/acquire fails |
| `lchown03_16` | 0/3 | fail | test device image creation/acquire fails |
| `lgetxattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `lgetxattr02` | 0/1 | fail | xattr unsupported/mismatch |
| `link01` | 0/2 | fail | symlink/lstat setup breaks in symlink01 alias |
| `link02` | 1/2 | partial | hard link succeeds but link count remains `1` |
| `link04` | 10/14 | partial | bad address errno and directory permission checks mismatch |
| `link05` | 0/1 | fail | 1000 hard links leave stat link count at `1` |
| `link08` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `linkat01` | 22/22 | pass | 2026-05-25 VFS run; cross-FS hard link returns `EXDEV` |
| `linkat02` | 0/3 | fail | test device image creation/acquire fails |
| `listxattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `listxattr02` | 0/1 | fail | xattr unsupported/mismatch |
| `listxattr03` | 0/1 | fail | xattr unsupported/mismatch |
| `llistxattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `llistxattr02` | 0/1 | fail | xattr unsupported/mismatch |
| `llistxattr03` | 0/1 | fail | xattr unsupported/mismatch |
| `lremovexattr01` | 0/2 | fail | test device image creation/acquire fails |
| `lstat01` | 0/1 | fail | symlink lstat reports incorrect values |
| `lstat01A` | 1/3 | partial | symlink01 alias: object-file lstat passes, symlink cases break |
| `lstat01A_64` | 1/3 | partial | symlink01 alias: object-file lstat passes, symlink cases break |
| `lstat01_64` | 0/1 | fail | symlink lstat reports incorrect values |
| `lstat02` | 4/6 | partial | bad-address/permission errno mismatches; ELOOP path cleanup warning |
| `lstat02_64` | 4/6 | partial | bad-address/permission errno mismatches; ELOOP path cleanup warning |
| `mkdir02` | 0/2 | fail | setgid directory inheritance/group ownership mismatch |
| `mkdir03` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `mkdir04` | 0/1 | fail | nested mkdir unexpectedly succeeds |
| `mkdir05` | 1/1 | pass |  |
| `mkdir09` | 0/2 | fail | test device image creation/acquire fails |
| `mkdirat01` | 5/5 | pass | 2026-05-25 VFS run |
| `mkdirat02` | 0/1 | fail | setup hits `mkdir(mntpoint/dir/) = EEXIST` |
| `mknod01` | 7/7 | pass |  |
| `mknod02` | 2/2 | pass |  |
| `mknod03` | 0/1 | fail | incorrect group ownership |
| `mknod04` | 0/1 | fail | incorrect group ownership |
| `mknod05` | 1/1 | pass |  |
| `mknod06` | 5/6 | partial | bad user pointer returns `ENAMETOOLONG`, expected `EFAULT` |
| `mknod07` | 0/3 | fail | test device image creation/acquire fails |
| `mknod08` | 1/1 | pass |  |
| `mknod09` | 1/1 | pass |  |
| `mknodat01` | 5/5 | pass | 2026-05-25 VFS run |
| `mknodat02` | 0/3 | fail | test device image creation/acquire fails |
| `name_to_handle_at01` | 27/27 | pass | 2026-05-25 VFS run; local txv2 file handle |
| `name_to_handle_at02` | 9/9 | pass | 2026-05-25 VFS run; errno coverage |
| `open01` | 2/2 | pass |  |
| `open01A` | 0/5 | fail | symlink01 alias/lstat setup breaks |
| `open02` | 1/2 | partial | unprivileged `O_NOATIME` unexpectedly succeeds |
| `open03` | 1/1 | pass |  |
| `open04` | 1/1 | pass |  |
| `open06` | 0/1 | fail | observed before the earlier host timeout; FIFO `O_NONBLOCK|O_WRONLY` unexpectedly succeeds |
| `open07` | 1/5 | partial | TFAIL: open(O_NOFOLLOW) a symlink to file succeeded |
| `open08` | 2/6 | partial | TFAIL: O_RDWR succeeded |
| `open09` | 2/2 | pass |  |
| `open10` | 6/9 | partial | TFAIL: dir_b/nosetgid: Incorrect group, 65534 != 1 |
| `open11` | 28/28 | pass | focused RV/LA musl/glibc rerun passes after directory `EISDIR` handling |
| `open12` | 3/5 | partial | TBROK: open12.c:224: write(3,0xe56188,11) failed: errno=EINVAL(22): Invalid argument |
| `open13` | 2/5 | partial | TFAIL: open13.c:144: fchmod(2) succeeded unexpectedly |
| `open14` | 0/2 | fail | TBROK: open14.c:68: write(3,0x3b5b78,1024) failed: errno=EISDIR(21): Is a directory |
| `open_by_handle_at01` | 9/9 | pass |  |
| `open_by_handle_at02` | 7/7 | pass | EINVAL observed |
| `openat01` | 0/1 | fail | TBROK: mkdir(test_dir/, 0700) failed: EEXIST (17) |
| `openat02` | 2/4 | partial | TBROK: openat02.c:199: write(3,0x9d2368,7) failed: errno=EINVAL(22): Invalid argument |
| `openat03` | 0/2 | fail | TBROK: openat03.c:79: write(3,0x3b5b78,1024) failed: errno=EISDIR(21): Is a directory |
| `openat04` | 0/2 | fail | TBROK: Failed to acquire device |
| `openat201` | 0/1 | skip | TCONF: syscall(437) __NR_openat2 not supported on your arch |
| `openat202` | 0/1 | skip | TCONF: syscall(437) __NR_openat2 not supported on your arch |
| `openat203` | 0/1 | skip | TCONF: syscall(437) __NR_openat2 not supported on your arch |
| `prot_hsymlinks` | 396/397 | partial | TWARN: tst_tmpdir.c:342: tst_rmdir: rmobj(/tmp/LTP_progfCknd) failed: remove(/tmp/LTP_progfCknd) failed; errno=39: ENOTEMPTY |
| `readdir01` | 1/1 | pass |  |
| `readdir21` | 0/1 | skip | TCONF: syscall(-1) __NR_readdir not supported on your arch |
| `readlink01` | 2/2 | pass |  |
| `readlink01A` | 2/4 | partial | TBROK: symlink01.c:986: lstat(2) Failure when accessing symbolic symbolic link file which should contain object path to (null) file |
| `readlink03` | 7/8 | partial | TFAIL: readlink() sueeeeded unexpectedly |
| `readlinkat01` | 10/12 | partial | TFAIL: readlinkat(5, , , 1024) failed: EINVAL (22) |
| `readlinkat02` | 6/6 | pass | EINVAL observed |
| `removexattr01` | 0/1 | fail | TFAIL: removexattr01.c:80: setxattr() failed: errno=ENOSYS(38): Function not implemented |
| `removexattr02` | 0/3 | fail | TFAIL: removexattr02.c:99: removexattr() failed unexpectedly, expected ENODATA: TEST_ERRNO=ENOSYS(38): Function not implemented |
| `rename01` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename01A` | 0/2 | fail | TBROK: symlink01.c:986: lstat(2) Failure when accessing symbolic symbolic link file which should contain object path to (null) file |
| `rename03` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename04` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename05` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename06` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename07` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename08` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename09` | 0/1 | fail | TFAIL: rename() succeeded |
| `rename10` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename11` | 0/3 | fail | TBROK: tst_device.c:354: Failed to acquire device |
| `rename12` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename13` | 0/2 | fail | TBROK: Failed to acquire device |
| `rename14` | 0/0 | hang | single-case rerun still hit host timeout after `RUN LTP CASE rename14` with no further LTP output |
| `renameat01` | 0/3 | fail | single-case rerun exits cleanly; `tst_device.c` fails to create/acquire test device (`EINVAL`) |
| `renameat201` | 0/2 | fail | single-case rerun exits cleanly; setup `mkdir(test_dir/)` returns `EEXIST` |
| `renameat202` | 0/2 | fail | single-case rerun exits cleanly; setup `mkdir(test_dir/)` returns `EEXIST` |
| `rmdir01` | 1/1 | pass | single-case rerun passes |
| `rmdir02` | 0/1 | fail | TBROK: mkdir(mntpoint/dir/, 0777) failed: EEXIST (17) |
| `rmdir03` | 0/2 | fail | TFAIL: rmdir() succeeded unexpectedly |
| `rmdir03A` | 0/1 | fail | TBROK: symlink01.c:943: lstat(2) Failure when accessing symbolic symbolic link file which should contain object path to (null) file |
| `setxattr01` | 0/2 | fail | TBROK: Failed to acquire device |
| `setxattr02` | 0/3 | fail | TBROK: setxattr(setxattr02symlink, user.testkey, 0x44a320, 20) failed: ENOSYS (38) |
| `setxattr03` | 0/2 | fail | TBROK: Set setxattr03immutable immutable failed: ENOTTY (25) |
| `stat01` | 12/12 | pass |  |
| `stat01_64` | 12/12 | pass |  |
| `stat02` | 2/2 | pass |  |
| `stat02_64` | 2/2 | pass |  |
| `stat03` | 4/6 | partial | TFAIL: stat(tc->pathname, &stat_buf) succeeded |
| `stat03_64` | 4/6 | partial | TFAIL: stat(tc->pathname, &stat_buf) succeeded |
| `stat04` | 0/3 | fail | TBROK: symlink01.c:986: symbolic is not a symbolic link file which contains object path to object file |
| `stat04_64` | 0/3 | fail | TBROK: symlink01.c:986: symbolic is not a symbolic link file which contains object path to object file |
| `statfs01` | 0/2 | fail | TBROK: Failed to acquire device |
| `statfs01_64` | 0/2 | fail | TBROK: Failed to acquire device |
| `statfs02` | 1/6 | partial | TFAIL: statfs() succeeded |
| `statfs02_64` | 1/6 | partial | TFAIL: statfs() succeeded |
| `statfs03` | 0/1 | fail | TFAIL: statfs(TEMP_DIR2, &buf) succeeded |
| `statfs03_64` | 0/1 | fail | TFAIL: statfs(TEMP_DIR2, &buf) succeeded |
| `statvfs01` | 0/2 | fail | TBROK: Failed to acquire device |
| `statvfs02` | 0/5 | fail | TFAIL: statvfs(tc->path, tc->buf) succeeded |
| `statx01` | 0/1 | fail | TBROK: mkdir(mntpoint/, 0777) failed: EEXIST (17) |
| `statx02` | 4/5 | partial | TFAIL: Statx symlink flag failed to work as expected |
| `statx03` | 5/7 | partial | TFAIL: statx() should fail with EFAULT: ENAMETOOLONG (36) |
| `statx04` | 0/2 | fail | TBROK: Failed to acquire device |
| `statx05` | 0/1 | skip | TCONF: Couldn't find 'mkfs.ext4' in $PATH |
| `statx06` | 0/2 | fail | TBROK: Failed to acquire device |
| `statx07` | 0/1 | skip | TCONF: Couldn't find 'exportfs' in $PATH |
| `statx08` | 0/2 | fail | TBROK: Failed to acquire device |
| `statx09` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `statx10` | 0/2 | fail | TBROK: Failed to acquire device |
| `statx11` | 0/2 | fail | TBROK: Failed to acquire device |
| `statx12` | 0/2 | fail | TBROK: Failed to acquire device |
| `symlink01` | 1/5 | partial | TBROK: symlink01.c:983: lstat(2) Failure when accessing symbolic symbolic link file which should contain %bc+eFhi!k path to (null) file |
| `symlink02` | 1/1 | pass |  |
| `symlink03` | 4/6 | partial | TFAIL: symlink03.c:189: symlink() returned 0, expected -1, errno:13 |
| `symlink04` | 2/4 | partial | TBROK: lstat(slink_file,0x40202b70) failed: ENOENT (2) |
| `symlinkat01` | 10/10 | pass |  |
| `truncate02` | 2/2 | pass |  |
| `truncate02_64` | 2/2 | pass |  |
| `truncate03` | 5/8 | partial | TFAIL: truncate(tc->pathname, tc->length) succeeded |
| `truncate03_64` | 5/8 | partial | TFAIL: truncate(tc->pathname, tc->length) succeeded |
| `umask01` | 1/1 | pass |  |
| `unlink01` | 0/1 | fail | TBROK: symlink01.c:986: symbolic is not a symbolic link file which contains object path to object file |
| `unlink05` | 2/2 | pass |  |
| `unlink07` | 5/6 | partial | TFAIL: invalid address expected EFAULT: ENAMETOOLONG (36) |
| `unlink08` | 2/4 | partial | TFAIL: unwritable directory succeeded |
| `unlink09` | 0/1 | fail | TBROK: mkdir(erofs/dir/, 0777) failed: EEXIST (17) |
| `unlinkat01` | 7/7 | pass | EINVAL observed |
| `utime01` | 0/2 | fail | TBROK: Failed to acquire device |
| `utime02` | 0/2 | fail | TBROK: Failed to acquire device |
| `utime03` | 0/2 | fail | TBROK: Failed to acquire device |
| `utime04` | 0/2 | fail | TBROK: Failed to acquire device |
| `utime05` | 0/2 | fail | TBROK: Failed to acquire device |
| `utime06` | 0/1 | fail | TBROK: mkdir(mntpoint/dir/, 0777) failed: EEXIST (17) |
| `utime07` | 0/1 | fail | TBROK: symlink generated a non-symbolic link my_symlink0 to /tmp/LTP_utiaCGcPk |
| `utimensat01` | 0/2 | fail | TBROK: Failed to acquire device |
| `utimes01` | 0/1 | fail | TBROK: mkdir(mntpoint/dir/, 0777) failed: EEXIST (17) |
