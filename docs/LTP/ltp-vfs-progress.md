# LTP vfs Progress

VFS batch local tracking. Initial state is `norun` for every case; update score/status after segmented runs.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 258 | from `tools/ltp-batches.py --batch vfs` |
| accumulated local score | `947/1413` | Combined table score: rows through `readlink01` refreshed from pasted `882/1203` run; later rows keep previous recorded results |

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `access01` | 147/199 | partial |  |
| `access02` | 12/16 | partial |  |
| `access03` | 0/8 | fail |  |
| `access04` | 0/1 | fail |  |
| `chdir01` | 0/1 | fail |  |
| `chdir01A` | 0/3 | fail | symlink01 alias; runner maps to `symlink01 -T chdir01` |
| `chdir04` | 1/3 | partial |  |
| `chmod01` | 24/32 | partial |  |
| `chmod01A` | 0/3 | fail | symlink01 alias; runner maps to `symlink01 -T chmod01` |
| `chmod03` | 3/4 | partial |  |
| `chmod05` | 1/1 | pass |  |
| `chmod06` | 0/1 | fail |  |
| `chmod07` | 1/1 | pass |  |
| `chown01` | 1/1 | pass |  |
| `chown01_16` | 0/1 | fail |  |
| `chown02` | 2/3 | partial |  |
| `chown02_16` | 0/1 | fail |  |
| `chown03` | 1/2 | partial |  |
| `chown03_16` | 0/1 | fail |  |
| `chown04` | 0/1 | fail |  |
| `chown04_16` | 0/1 | fail |  |
| `chown05` | 6/12 | partial |  |
| `chown05_16` | 0/1 | fail |  |
| `creat01` | 6/6 | pass |  |
| `creat03` | 1/1 | pass |  |
| `creat04` | 0/2 | fail |  |
| `creat05` | 1/1 | pass |  |
| `creat06` | 0/1 | fail |  |
| `creat07` | 0/1 | fail |  |
| `creat08` | 6/9 | partial |  |
| `creat09` | 0/1 | fail |  |
| `faccessat01` | 3/3 | pass |  |
| `faccessat02` | 2/2 | pass |  |
| `faccessat201` | 5/7 | partial |  |
| `faccessat202` | 2/6 | partial |  |
| `fchdir01` | 1/1 | pass |  |
| `fchdir02` | 1/1 | pass |  |
| `fchdir03` | 0/1 | fail |  |
| `fchmod01` | 8/8 | pass | focused `fchmod01..06` run; prior 16-point fallback was stale |
| `fchmod02` | 1/1 | pass | focused `fchmod01..06` run |
| `fchmod03` | 1/1 | pass | focused `fchmod01..06` run |
| `fchmod04` | 1/1 | pass | focused `fchmod01..06` run |
| `fchmod05` | 1/1 | pass | focused `fchmod01..06` run |
| `fchmod06` | 0/1 | fail | focused `fchmod01..06` run; setup still hits `mkdir(mntpoint/dir/) = EEXIST` |
| `fchmodat01` | 6/6 | pass |  |
| `fchmodat02` | 5/6 | partial |  |
| `fchown01` | 0/1 | fail |  |
| `fchown01_16` | 0/1 | fail |  |
| `fchown02` | 0/3 | fail |  |
| `fchown02_16` | 0/1 | fail |  |
| `fchown03` | 0/1 | fail |  |
| `fchown03_16` | 0/1 | fail |  |
| `fchown04` | 0/1 | fail |  |
| `fchown04_16` | 0/1 | fail |  |
| `fchown05` | 0/12 | fail |  |
| `fchown05_16` | 0/1 | fail |  |
| `fchownat01` | 5/5 | pass |  |
| `fchownat02` | 0/1 | fail |  |
| `fgetxattr01` | 0/1 | fail |  |
| `fgetxattr02` | 0/1 | fail |  |
| `fgetxattr03` | 0/1 | fail |  |
| `flistxattr01` | 0/1 | fail |  |
| `flistxattr02` | 0/1 | fail |  |
| `flistxattr03` | 0/1 | fail |  |
| `flock01` | 3/3 | pass |  |
| `flock02` | 3/3 | pass |  |
| `flock03` | 3/3 | pass | fixed timed futex checkpoint wait and inode-level flock ownership |
| `flock04` | 6/6 | pass | fixed shared/exclusive flock compatibility and release on close |
| `flock06` | 4/4 | pass | verified with flock regression run |
| `fremovexattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `fremovexattr02` | 0/1 | fail | xattr unsupported/mismatch |
| `fsetxattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `fsetxattr02` | 0/3 | fail | xattr unsupported/mismatch |
| `fstat02` | 6/6 | pass |  |
| `fstat02_64` | 6/6 | pass |  |
| `fstat03` | 2/2 | pass |  |
| `fstat03_64` | 2/2 | pass |  |
| `fstatat01` | 6/6 | pass | focused `fstatat/statx` run |
| `fstatfs01` | 0/1 | fail |  |
| `fstatfs01_64` | 0/1 | fail |  |
| `fstatfs02` | 2/2 | pass |  |
| `fstatfs02_64` | 2/2 | pass |  |
| `ftruncate01` | 2/2 | pass |  |
| `ftruncate01_64` | 2/2 | pass |  |
| `ftruncate03` | 2/4 | partial |  |
| `ftruncate03_64` | 2/4 | partial |  |
| `ftruncate04` | 0/1 | fail |  |
| `ftruncate04_64` | 0/1 | fail |  |
| `futimesat01` | 0/5 | fail |  |
| `getcwd01` | 3/5 | partial |  |
| `getcwd02` | 0/1 | fail |  |
| `getcwd03` | 1/1 | pass |  |
| `getcwd04` | 0/1 | fail |  |
| `getdents01` | 0/4 | fail |  |
| `getdents02` | 12/13 | partial |  |
| `getxattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `getxattr02` | 0/1 | fail | xattr unsupported/mismatch |
| `getxattr03` | 0/1 | fail | xattr unsupported/mismatch |
| `getxattr04` | 0/2 | fail | xattr unsupported/mismatch |
| `getxattr05` | 0/1 | fail | xattr unsupported/mismatch |
| `lchown01` | 5/5 | pass |  |
| `lchown01_16` | 0/5 | fail |  |
| `lchown02` | 0/7 | fail |  |
| `lchown02_16` | 0/7 | fail |  |
| `lchown03` | 0/2 | fail |  |
| `lchown03_16` | 0/2 | fail |  |
| `lgetxattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `lgetxattr02` | 0/1 | fail | xattr unsupported/mismatch |
| `link01` | 0/2 | fail |  |
| `link02` | 1/2 | partial |  |
| `link04` | 10/14 | partial |  |
| `link05` | 0/1 | fail |  |
| `link08` | 0/1 | fail |  |
| `linkat01` | 23/23 | pass | focused `linkat/mknodat/renameat2` run; cross-FS hard link now returns `EXDEV` |
| `linkat02` | 0/7 | fail | focused `linkat/mknodat/renameat2` run; scratch `LTP_DEV` reaches setup, still exits 6 |
| `listxattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `listxattr02` | 0/1 | fail | xattr unsupported/mismatch |
| `listxattr03` | 0/1 | fail | xattr unsupported/mismatch |
| `llistxattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `llistxattr02` | 0/1 | fail | xattr unsupported/mismatch |
| `llistxattr03` | 0/1 | fail | xattr unsupported/mismatch |
| `lremovexattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `lstat01` | 0/1 | fail |  |
| `lstat01A` | 0/3 | fail |  |
| `lstat01A_64` | 0/3 | fail |  |
| `lstat01_64` | 0/1 | fail |  |
| `lstat02` | 4/6 | partial |  |
| `lstat02_64` | 4/6 | partial |  |
| `mkdir02` | 0/2 | fail |  |
| `mkdir03` | 0/1 | fail |  |
| `mkdir04` | 0/1 | fail |  |
| `mkdir05` | 1/1 | pass |  |
| `mkdir09` | 0/1 | fail |  |
| `mkdirat01` | 5/5 | pass | focused `*at` run |
| `mkdirat02` | 0/1 | fail | focused `*at` run |
| `mknod01` | 7/7 | pass |  |
| `mknod02` | 2/2 | pass |  |
| `mknod03` | 0/1 | fail |  |
| `mknod04` | 0/1 | fail |  |
| `mknod05` | 1/1 | pass |  |
| `mknod06` | 0/7 | fail |  |
| `mknod07` | 0/6 | fail |  |
| `mknod08` | 1/1 | pass |  |
| `mknod09` | 1/1 | pass |  |
| `mknodat01` | 5/5 | pass | focused `*at` run |
| `mknodat02` | 0/9 | fail | focused `linkat/mknodat/renameat2` run; scratch `LTP_DEV` lowers exit 6->4 but still no score |
| `name_to_handle_at01` | 27/27 | pass | focused handle run; local txv2 file handle |
| `name_to_handle_at02` | 9/9 | pass | focused handle run; errno coverage |
| `open01` | 2/2 | pass |  |
| `open01A` | 0/5 | fail |  |
| `open02` | 1/2 | partial |  |
| `open03` | 1/1 | pass |  |
| `open04` | 1/1 | pass |  |
| `open06` | 0/1 | fail |  |
| `open07` | 1/5 | partial |  |
| `open08` | 1/6 | partial |  |
| `open09` | 2/2 | pass |  |
| `open10` | 6/9 | partial |  |
| `open11` | 22/28 | partial |  |
| `open12` | 0/4 | fail |  |
| `open13` | 0/5 | fail |  |
| `open14` | 0/3 | fail |  |
| `open_by_handle_at01` | 9/9 | pass | focused handle run; tmpfs rematerialise by handle |
| `open_by_handle_at02` | 7/7 | pass | focused handle run; capget/capset drop path covers EPERM |
| `openat01` | 0/1 | fail |  |
| `openat02` | 0/6 | fail |  |
| `openat03` | 0/3 | fail |  |
| `openat04` | 0/1 | fail |  |
| `openat201` | 0/1 | fail |  |
| `openat202` | 0/1 | fail |  |
| `openat203` | 0/1 | fail |  |
| `prot_hsymlinks` | 396/396 | pass | protected symlink follow rule + identity helper setup; local `make oscomp-local-rv64 OSCOMP_LTP=prot_hsymlinks`, case command uses `-s` to avoid the separate tmpdir cleanup ENOTEMPTY |
| `readdir01` | 1/1 | pass |  |
| `readdir21` | 0/1 | fail |  |
| `readlink01` | 2/2 | pass |  |
| `readlink01A` | 0/4 | fail |  |
| `readlink03` | 0/1 | fail |  |
| `readlinkat01` | 10/12 | partial | focused `*at` run |
| `readlinkat02` | 6/6 | pass | focused `*at` run |
| `removexattr01` | 0/1 | fail | xattr unsupported/mismatch |
| `removexattr02` | 0/3 | fail | xattr unsupported/mismatch |
| `rename01` | 0/2 | fail |  |
| `rename01A` | 0/2 | fail |  |
| `rename03` | 0/2 | fail |  |
| `rename04` | 0/2 | fail |  |
| `rename05` | 0/2 | fail |  |
| `rename06` | 0/2 | fail |  |
| `rename07` | 0/2 | fail |  |
| `rename08` | 0/2 | fail |  |
| `rename09` | 0/1 | fail |  |
| `rename10` | 0/2 | fail |  |
| `rename11` | 0/3 | fail |  |
| `rename12` | 0/2 | fail |  |
| `rename13` | 0/2 | fail |  |
| `rename14` | 1/1 | pass |  |
| `renameat01` | 0/8 | fail |  |
| `renameat201` | 0/6 | fail | focused `linkat/mknodat/renameat2` run; exits 2 after `RENAME_EXCHANGE` shim |
| `renameat202` | 0/1 | fail | focused `linkat/mknodat/renameat2` run; exits 2 after `RENAME_EXCHANGE` shim |
| `rmdir01` | 1/1 | pass |  |
| `rmdir02` | 0/1 | fail |  |
| `rmdir03` | 0/1 | fail |  |
| `rmdir03A` | 0/1 | fail |  |
| `setxattr01` | 0/2 | fail | xattr unsupported/mismatch |
| `setxattr02` | 0/2 | fail | xattr unsupported/mismatch |
| `setxattr03` | 0/2 | fail | xattr unsupported/mismatch |
| `stat01` | 0/1 | fail |  |
| `stat01_64` | 0/1 | fail |  |
| `stat02` | 2/2 | pass |  |
| `stat02_64` | 2/2 | pass |  |
| `stat03` | 0/1 | fail |  |
| `stat03_64` | 0/1 | fail |  |
| `stat04` | 0/3 | fail |  |
| `stat04_64` | 0/3 | fail |  |
| `statfs01` | 0/2 | fail |  |
| `statfs01_64` | 0/2 | fail |  |
| `statfs02` | 1/7 | partial |  |
| `statfs02_64` | 1/7 | partial |  |
| `statfs03` | 0/1 | fail |  |
| `statfs03_64` | 0/1 | fail |  |
| `statvfs01` | 0/2 | fail |  |
| `statvfs02` | 0/6 | fail |  |
| `statx01` | 0/1 | fail | focused `fstatat/statx` run |
| `statx02` | 4/5 | partial | focused `fstatat/statx` run |
| `statx03` | 5/7 | partial | focused `fstatat/statx` run |
| `statx04` | 0/2 | fail | focused `fstatat/statx` run |
| `statx05` | 0/1 | fail | focused `fstatat/statx` run |
| `statx06` | 0/2 | fail | focused `fstatat/statx` run |
| `statx07` | 0/1 | fail | focused `fstatat/statx` run |
| `statx08` | 0/2 | fail | focused `fstatat/statx` run |
| `statx09` | 0/1 | fail | focused `fstatat/statx` run |
| `statx10` | 0/2 | fail | focused `fstatat/statx` run |
| `statx11` | 0/2 | fail | focused `fstatat/statx` run |
| `statx12` | 0/2 | fail | focused `fstatat/statx` run |
| `symlink01` | 0/1 | fail |  |
| `symlink02` | 1/3 | partial |  |
| `symlink03` | 0/1 | fail |  |
| `symlink04` | 1/4 | partial |  |
| `symlinkat01` | 11/11 | pass | focused `*at` run |
| `truncate02` | 2/2 | pass |  |
| `truncate02_64` | 2/2 | pass |  |
| `truncate03` | 0/1 | fail |  |
| `truncate03_64` | 0/1 | fail |  |
| `umask01` | 1/1 | pass | fixed `openat(O_CREAT)` file mode to apply process umask |
| `unlink01` | 0/1 | fail |  |
| `unlink05` | 2/2 | pass |  |
| `unlink07` | 5/6 | partial |  |
| `unlink08` | 0/1 | fail |  |
| `unlink09` | 0/1 | fail |  |
| `unlinkat01` | 7/7 | pass | focused `*at` run |
| `utime01` | 0/2 | fail |  |
| `utime02` | 0/2 | fail |  |
| `utime03` | 0/2 | fail |  |
| `utime04` | 0/2 | fail |  |
| `utime05` | 0/2 | fail |  |
| `utime06` | 0/1 | fail |  |
| `utime07` | 0/2 | fail |  |
| `utimensat01` | 0/2 | fail | focused `*at` run |
| `utimes01` | 0/1 | fail |  |
