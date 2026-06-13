# LTP LA64 Submit Whitelist

This file is the LA64-specific view of `docs/LTP/ltp-submit-whitelist.md`.
It includes every historical non-network whitelist case. Socket/network rows
remain documented below as audit data only, but they are not appended to the
active LA submit or submit-glibc LTP runner.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| shared whitelist rows | 651 | 609 historical non-network rows + 42 RV network positive-score rows |
| LA active submit rows | 451 | filtered non-network rows plus the 2026-06-03 and 2026-06-04 submit-tail promotions; LA network rows are audit-only |
| LA active submit-glibc rows | 451 | same non-network rows as LA submit; no LA network rows |
| LA excluded/non-submit rows | historical tail audit rows + network audit rows | active submit stops broad historical tail at `io_uring01`, then appends only verified promoted tail cases |
| LA submit stitched score | pending clean rerun | previous `4277/5094` included network rows and should not be used for current LA submit |
| source whitelist | `docs/LTP/ltp-submit-whitelist.md` | generated from the integrated `Cases` table |
| network source | `docs/ljs/LTP_NETWORK_CURRENT_SCOREBOARD_2026-06-02.md` | `sendmsg*` skipped in the current network sweep |
| local glibc-only command | `make oscomp-local-la64-ltp-batch LTP_BATCH=submit-glibc` | uses the short batch name so the case list is expanded in-kernel instead of through a long qemu cmdline |

## 2026-06-03 Submit-Tail Promotion

LA64 focused runs verified a non-network tail subset from the historical
post-`io_uring01` table and added it to the active submit runner through
`crates/tx-kernel/src/init/exec.rs::LTP_SUBMIT_PROMOTED_TAIL_CASES`.

- LA musl promoted-candidate run: `92/92`; active promoted subset: `91/91`
  after dropping `write01`.
- LA glibc promoted-candidate run: `91/92`; `write01` failed with `EINVAL`,
  so the active promoted subset is `91/91`.
- Logs:
  `target/oscomp/ltp-extra-core-a-la-musl-20260603.txt`,
  `target/oscomp/ltp-extra-core-a2-la-musl-20260603.txt`,
  `target/oscomp/ltp-extra-core-b1-la-musl-20260603.txt`,
  `target/oscomp/ltp-extra-core-b2-la-musl-20260603.txt`,
  `target/oscomp/ltp-extra-core-c-la-musl-20260603.txt`,
  `target/oscomp/ltp-extra-core-g1-la-glibc-20260603.txt`,
  `target/oscomp/ltp-extra-core-g2-la-glibc-20260603.txt`,
  `target/oscomp/ltp-extra-core-g3-la-glibc-20260603.txt`,
  `target/oscomp/ltp-extra-core-g4-la-glibc-20260603.txt`, and
  `target/oscomp/ltp-extra-core-g5-la-glibc-20260603.txt`.

## 2026-06-03 High-Score Focused Fixes

Two existing LA whitelist cases improved in both musl and glibc focused runs:

- `open11`: `23/28` -> `28/28`; directory write opens and `O_CREAT` against
  existing directories now return `EISDIR`.
- `semop02`: `19/26` -> `21/26`; `nsops > SEMOPM` now returns `E2BIG`.

Logs:
`target/oscomp/ltp-highfix-open11-semop02-la-musl-20260603.txt`,
`target/oscomp/ltp-highfix-open11-la-musl-20260603.txt`, and
`target/oscomp/ltp-highfix-open11-semop02-la-glibc-20260603.txt`.

## 2026-06-04 Full-Pass Local Additions

The LA side was checked with focused local musl runs, without `LTP_MAX_RUNTIME`
and without synthesized Summary blocks. Only rows that the judge reports as
full-pass are promoted.

New LA submit-tail candidates:

`getpriority01`, `getpriority02`, `nice01`, `nice02`, `nice03`, `nice04`,
`prctl01`, `prctl09`, `sched_get_priority_max01`,
`sched_get_priority_max02`, `sched_get_priority_min01`,
`sched_get_priority_min02`, `sched_rr_get_interval01`, `setpriority02`,
`wait402`, `wait02`, `wait01`, `shmat04`, `sendfile08_64`, `sendfile08`,
`sendfile06_64`, `sendfile06`, `sendfile05_64`, `sendfile05`, `semop04`,
`semctl02`, `pidfd_open01`, `personality02`, `msgrcv08`, `msgget01`,
`mknod09`, `kill06`, `getsid02`, `getsid01`, `getppid02`, `getppid01`,
`fork08`, `fork07`, `fork03`, `exit02`, `setrlimit04`, `setrlimit05`,
`clone07`, `clone06`, `clone05`, and `clone03`.

These 46 rows are `83/83` on the LA musl focused runs. The matching RV glibc
candidate sweep validated most of the same subset, but the 2026-06-04 RV full
submit rerun timed out at glibc `exit_group01` after the thread-exit check and
ended with `trap-action-terminate`; that row is now audit-only and the
remaining promoted glibc-safe subset is `79/79` pending the next clean full
rerun. The `exec*` rows pass LA/RV musl but failed in that glibc sweep, so they
are not promoted yet.

Not promoted: partial `prctl02`/`prctl08`, LA `sched_setparam*` and related
rows with `TCONF`/failure, RV glibc full-submit-timeout `exit_group01`,
old-format `0/0` rows, and the already excluded slow or unstable cases such as
`fcntl36*`, `fcntl34*`, `futex_wait03`, `semop05`, `chmod05`, and `mknod05`.

Logs:

`target/oscomp/ltp-sched-prctl-a-la-musl-noI-20260603.txt`,
`target/oscomp/ltp-sched-extra-a-la-musl-noI-20260604.txt`,
`target/oscomp/ltp-tail-onepoint-a-la-musl-noI-20260604.txt`,
`target/oscomp/ltp-tail-onepoint-b-la-musl-noI-20260604.txt`,
`target/oscomp/ltp-tail-onepoint-c-la-musl-noI-20260604.txt`,
`target/oscomp/ltp-tail-onepoint-d-la-musl-noI-20260604.txt`,
`target/oscomp/ltp-tail-onepoint-e-la-musl-noI-20260604.txt`, and
`target/oscomp/os_serial_out_rv.txt` for the RV glibc focused candidate run.

## Non-Submit Cases

These rows are kept for auditability but are not LA submit candidates right now.
For shared legacy rows, the historical `LA Submit` value in the table should be
read as audit-only until the table is regenerated.

Shared official-unscored legacy exclusions from `LTP_SUBMIT_UNSCORED_LEGACY_CASES`:

`prot_hsymlinks`, `clone02`, `fallocate01`, `fallocate02`, `fchownat01`,
`fcntl07`, `fcntl07_64`, `fcntl09`, `fcntl09_64`, `fcntl10`, `fcntl10_64`,
`fstatat01`, `get_robust_list01`, `kill02`, `lchown01`, `lchown02`,
`linkat01`, `mincore01`, `mkdirat01`, `mknod06`, `mknodat01`, `mlockall01`,
`mlockall03`, `mremap05`, `msync03`, `munmap03`, `open12`, `open13`,
`openat02`, `readlink01`, `rt_sigaction01`, `rt_sigaction02`,
`rt_sigaction03`, `rt_sigprocmask02`, `sched_getattr02`, `sched_setattr01`,
`setresgid01`, `setrlimit01`, `setsid01`, `signalfd01`, `symlink03`,
`symlinkat01`, `sysconf01`, `ulimit01`.

Additional LA-only non-network exclusions from `LTP_LA_SUBMIT_EXCLUDED_CASES`:

`gettid02`, `fcntl36_64`, `fcntl36`, `creat08`, `open10`,
`futex_wait03`, `pselect01`, `pselect01_64`, `fcntl34`, `fcntl34_64`,
`mq_notify01`, `semop05`, `chmod05`, `mknod05`.

`fcntl36_64` and `fcntl36` are kept out of active submit for now: Txv2 still
lacks complete POSIX/OFD record-lock ownership separation and
`F_SETLKW`/`F_OFD_SETLKW` blocking-wakeup semantics, and the cases are slow in
combined runs.

`readv01` and `truncate03_64` were temporarily excluded after earlier
long-run `memory allocation of 2097152 bytes failed` panics. After the tmpfs
unlink/drop cleanup, the interrupted 2026-06-04 rerun reached well past
`readv01`, `truncate03_64`, and `stat03_64` without reproducing the 2 MiB
allocation panic, so both rows are back in the active LA submit list pending a
clean full rerun.

Network rows present in the shared RV-positive network table are now all
audit-only for LA submit.

`getsockopt02` passes as an LA single glibc case and in the earlier LA musl
network sweep, but the combined LA whitelist run reaches it in `ltp-glibc`
after `ltp-musl`, repeatedly reports `address is in use`, and then terminates.
That exposed socket close/port-release state leakage across the two libc groups.
After the 2026-06-03 long-run failures, LA submit and submit-glibc no longer
append any network cases. Keep focused network sweeps separate while the network
stack work is ongoing.

Additional network cases outside the shared positive-score table remain excluded:
`socketcall01`, `socketcall02`, `socketcall03`, `setsockopt05`,
`setsockopt07`, and `sendmsg*`.

`setsockopt05` is removed from the shared active submit whitelist. Focused runs
can pass, but repeated LA glibc full-whitelist runs on 2026-06-03 panic inside
this case with `memory allocation of 2097152 bytes failed`
(`target/oscomp/os_serial_out_la_whitelist_submit_glibc_clean.txt` and
`target/oscomp/os_serial_out_la_whitelist_submit_glibc_packet-send-range-fix.txt`).

## Cases

| Case | Module | Area | LA Status | LA Score | LA Submit | Note |
| --- | --- | --- | --- | ---: | --- | --- |
| `prot_hsymlinks` | vfs | vfs | partial | `396/397` | yes | LA 396/397; TWARN: tst_tmpdir.c:342: tst_rmdir: rmobj(/tmp/LTP_proNAhOKM) failed: remove(/tmp/LTP_proNAhOKM) failed; errno=39: ENOTEMPTY |
| `epoll_ctl03` | event | event | pass | `256/256` | yes | LA 256/256 |
| `splice07` | fd-io | fd-io | partial | `217/377` | yes | LA 217/377; TCONF: pidfd_open(): ENOSYS (38) |
| `rt_sigaction01` | signal | signal | pass | `150/150` | yes | LA 150/150 |
| `rt_sigaction02` | signal | signal | pass | `150/150` | yes | LA 150/150 |
| `rt_sigaction03` | signal | signal | pass | `150/150` | yes | LA 150/150 |
| `access01` | vfs | vfs | partial | `147/199` | yes | LA 147/199; TFAIL: access(accessfile_r, W_OK) as nobody succeeded |
| `getpid01` | process | process | pass | `100/100` | yes | LA 100/100 |
| `waitpid01` | process | process | partial | `84/115` | yes | LA 84/115; TFAIL: WIFSIGNALED() not set in status (exited with 0) |
| `pipe11` | fd-io | fd-io | pass | `70/70` | yes | LA 70/70 |
| `timer_settime02` | time | time | pass | `48/48` | yes | LA 48/48 |
| `clock_getres01` | time | time | pass | `44/44` | yes | LA 44/44 |
| `sysconf01` | smoke | libc | partial | `36/56` | yes | LA 36/56; TCONF: sysconf01.c:65: Not supported sysconf resource: _SC_CHILD_MAX |
| `posix_fadvise03` | fd-io | fd-io | pass | `32/32` | yes | LA 32/32 |
| `posix_fadvise03_64` | fd-io | fd-io | pass | `32/32` | yes | LA 32/32 |
| `confstr01` | smoke | libc | pass | `36/36` | yes | LA 36/36 |
| `timer_settime01` | time | time | pass | `32/32` | yes | LA 32/32 |
| `signal03` | signal | signal | pass | `31/31` | yes | LA 31/31 |
| `signal05` | signal | signal | partial | `30/31` | yes | LA 30/31; TFAIL: siglist[n] (18) != sig_pass (17) |
| `getitimer01` | time | time | pass | `30/30` | yes | LA 30/30 |
| `mq_timedsend01` | ipc | ipc | partial | `28/34` | yes | LA 28/34; TFAIL: mq_timedsend() failed unexpectedly, expected EINVAL: EAGAIN/EWOULDBLOCK (11) |
| `signal04` | signal | signal | pass | `28/28` | yes | LA 28/28 |
| `name_to_handle_at01` | vfs | vfs | pass | `27/27` | yes | LA 27/27 |
| `mq_timedreceive01` | ipc | ipc | partial | `24/30` | yes | LA 24/30; TFAIL: mq_timedreceive() failed unexpectedly, expected EINVAL: EAGAIN/EWOULDBLOCK (11) |
| `chmod01` | vfs | vfs | partial | `16/32` | yes | LA 16/32; TFAIL: stat(testfile) mode=0644 |
| `open11` | vfs | vfs | pass | `28/28` | yes | LA 28/28; focused musl/glibc rerun passes after directory `EISDIR` handling |
| `linkat01` | vfs | vfs | pass | `22/22` | yes | LA 22/22 |
| `semop02` | ipc | ipc | partial | `21/26` | yes | LA 21/26; `nsops > SEMOPM` now returns E2BIG; two variants still succeed unexpectedly |
| `ppoll01` | event | event | partial | `18/20` | yes | LA 18/20; TFAIL: ret: 0, exp: -1, ret_errno: SUCCESS (0), exp_errno: EINTR (4) |
| `llseek03` | fd-io | fd-io | pass | `18/18` | yes | LA 18/18 |
| `personality01` | process | process | pass | `18/18` | yes | LA 18/18 |
| `setitimer01` | time | time | pass | `18/18` | yes | LA 18/18 |
| `pathconf01` | smoke | fs | pass | `17/17` | yes | LA 17/17 |
| `setregid03` | cred | cred | partial | `16/22` | yes | LA 16/22; primary gid denial/saved gid checks mismatch |
| `select03` | event | event | partial | `16/40` | yes | LA 16/40; TCONF: syscall(-1) __NR_select not supported on your arch |
| `semctl07` | ipc | ipc | pass | `16/16` | yes | LA 16/16 |
| `shmctl02` | ipc | ipc | partial | `16/22` | yes | LA 16/22; TFAIL: shmctl(4, 11, 0x120038c38) expected EPERM: EINVAL (22) |
| `getrlimit01` | sched | sched | pass | `16/16` | yes | LA 16/16 |
| `getrlimit03` | sched | sched | pass | `16/16` | yes | LA 16/16 |
| `readahead01` | fd-io | fd-io | partial | `15/25` | yes | LA 15/25; TCONF: pidfd_open(): ENOSYS (38) |
| `select02` | event | event | partial | `14/17` | yes | LA 14/17; TCONF: syscall(-1) __NR_select not supported on your arch |
| `mmap04` | vm | vm | pass | `14/14` | yes | LA 14/14 |
| `msgctl01` | ipc | ipc | partial | `13/14` | yes | LA 13/14; TFAIL: msg_ctime = 0, expected 1779494408 |
| `msgctl04` | ipc | ipc | partial | `12/14` | yes | LA 12/14; TCONF: EFAULT is skipped for libc variant |
| `access02` | vfs | vfs | partial | `12/16` | yes | LA 12/16; TFAIL: execute file_x as root failed: SUCCESS (0) |
| `getdents02` | vfs | vfs | partial | `8/10` | yes | LA 8/10; TCONF: syscall(-1) __NR_getdents not supported on your arch |
| `stat01` | vfs | vfs | pass | `12/12` | yes | LA 12/12 |
| `stat01_64` | vfs | vfs | pass | `12/12` | yes | LA 12/12 |
| `setreuid05` | cred | cred | partial | `11/15` | yes | LA 11/15; saved uid and non-root setreuid checks mismatch |
| `futex_wake03` | event | event | pass | `11/11` | yes | LA 11/11 |
| `msgrcv07` | ipc | ipc | partial | `11/13` | yes | LA 11/13; TFAIL: MSG_EXCEPT didn't get MSGTYPE1 message |
| `gettid02` | process | process | fail | `0/1` | no | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `clock_nanosleep01` | time | time | partial | `11/14` | yes | LA 11/14; TFAIL: returned 0, expected -1, expected errno: EFAULT (14): SUCCESS (0) |
| `readv01` | fd-io | fd-io | pass | `10/10` | yes | LA 10/10; re-enabled for LA glibc after the 2026-06-04 interrupted rerun passed the previous 2 MiB allocation-panic point |
| `clock_gettime02` | time | time | pass | `10/10` | yes | LA 10/10 |
| `link04` | vfs | vfs | partial | `10/14` | yes | LA 10/14; TFAIL: link(<invalid address>, <nefile>) Failed expected errno: 14: ENAMETOOLONG (36) |
| `readlinkat01` | vfs | vfs | partial | `10/12` | yes | LA 10/12; TFAIL: readlinkat(5, , , 1024) failed: EINVAL (22) |
| `symlinkat01` | vfs | vfs | pass | `10/10` | yes | LA 10/10 |
| `setregid04` | cred | cred | pass | `9/9` | yes | LA 9/9 |
| `setresuid01` | cred | cred | pass | `9/9` | yes | LA 9/9 |
| `epoll_ctl02` | event | event | pass | `9/9` | yes | LA 9/9 |
| `epoll_wait06` | event | event | pass | `9/9` | yes | LA 9/9 |
| `lseek02` | fd-io | fd-io | partial | `9/15` | yes | LA 9/15; TFAIL: lseek(4, 1, 0) succeeded unexpectedly |
| `fpathconf01` | smoke | libc | pass | `9/9` | yes | LA 9/9 |
| `getrandom03` | smoke | random | pass | `9/9` | yes | LA 9/9 |
| `name_to_handle_at02` | vfs | vfs | pass | `9/9` | yes | LA 9/9 |
| `open_by_handle_at01` | vfs | vfs | pass | `9/9` | yes | LA 9/9 |
| `fallocate02` | fd-io | fd-io | pass | `8/8` | yes | LA 8/8 |
| `fallocate03` | fd-io | fd-io | pass | `8/8` | yes | LA 8/8 |
| `preadv02` | fd-io | fd-io | pass | `8/8` | yes | LA 8/8 |
| `preadv02_64` | fd-io | fd-io | pass | `8/8` | yes | LA 8/8 |
| `preadv202` | fd-io | fd-io | pass | `8/8` | yes | LA 8/8 |
| `preadv202_64` | fd-io | fd-io | pass | `8/8` | yes | LA 8/8 |
| `writev07` | fd-io | fd-io | pass | `8/8` | yes | LA 8/8 |
| `semctl01` | ipc | ipc | partial | `8/12` | yes | LA 8/12; TBROK: semctl(0, 0, 18,...) failed: EINVAL (22) |
| `semop03` | ipc | ipc | pass | `8/8` | yes | LA 8/8 |
| `sched_setscheduler01` | sched | sched | partial | `4/5` | yes | LA 4/5; TCONF: sched_setscheduler not supported |
| `timer_delete01` | time | time | pass | `8/8` | yes | LA 8/8 |
| `fchmod01` | vfs | vfs | pass | `8/8` | yes | LA 8/8 |
| `mmap06` | vm | vm | pass | `8/8` | yes | LA 8/8 |
| `setreuid01` | cred | cred | pass | `7/7` | yes | LA 7/7 |
| `setreuid02` | cred | cred | pass | `7/7` | yes | LA 7/7 |
| `epoll_wait02` | event | event | pass | `7/7` | yes | LA 7/7 |
| `futex_wait05` | event | event | pass | `7/7` | yes | LA 7/7 |
| `poll02` | event | event | pass | `7/7` | yes | LA 7/7 |
| `fcntl36_64` | fd-io | fd-io | fail | `0/1` | no | Active submit excludes this slow lock-wait case for now. POSIX/OFD record-lock ownership and blocking wait semantics are incomplete; LA 0/1; TBROK: Test killed by SIGSEGV! |
| `fcntl36` | fd-io | fd-io | fail | `0/1` | no | Active submit excludes this slow lock-wait case for now. POSIX/OFD record-lock ownership and blocking wait semantics are incomplete; LA 0/1; TBROK: Test killed by SIGSEGV! |
| `pipe2_01` | fd-io | fd-io | partial | `4/5` | yes | LA 4/5; TBROK: pipe2({-1,-1}) failed with flag(16384): EINVAL (22) |
| `pwritev02` | fd-io | fd-io | pass | `7/7` | yes | LA 7/7 |
| `pwritev02_64` | fd-io | fd-io | pass | `7/7` | yes | LA 7/7 |
| `pwritev202` | fd-io | fd-io | pass | `7/7` | yes | LA 7/7 |
| `pwritev202_64` | fd-io | fd-io | pass | `7/7` | yes | LA 7/7 |
| `clock_nanosleep02` | time | time | pass | `7/7` | yes | LA 7/7 |
| `nanosleep01` | time | time | pass | `7/7` | yes | LA 7/7 |
| `times03` | time | time | partial | `7/12` | yes | LA 7/12; TFAIL: buf1.tms_utime = 1157 |
| `mknod01` | vfs | vfs | pass | `7/7` | yes | LA 7/7 |
| `open_by_handle_at02` | vfs | vfs | pass | `7/7` | yes | LA 7/7 |
| `readlink03` | vfs | vfs | partial | `6/8` | yes | LA 6/8; TFAIL: readlink() sueeeeded unexpectedly |
| `unlinkat01` | vfs | vfs | pass | `7/7` | yes | LA 7/7 |
| `mremap05` | vm | vm | pass | `7/7` | yes | LA 7/7 |
| `capget01` | cred | cred | pass | `6/6` | yes | LA 6/6 |
| `setresgid02` | cred | cred | pass | `6/6` | yes | LA 6/6 |
| `futex_wake01` | event | event | pass | `6/6` | yes | LA 6/6 |
| `select01` | event | event | partial | `6/13` | yes | LA 6/13; TFAIL: select() with regular file timed out |
| `dup202` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `fcntl02` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `fcntl02_64` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `fcntl05` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `fcntl05_64` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `posix_fadvise01` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `posix_fadvise01_64` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `posix_fadvise02` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `posix_fadvise02_64` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `posix_fadvise04` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `posix_fadvise04_64` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `preadv201` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `preadv201_64` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `pwritev201` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `pwritev201_64` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `writev01` | fd-io | fd-io | pass | `6/6` | yes | LA 6/6 |
| `sethostname02` | heavy | heavy | pass | `6/6` | yes | LA 6/6 |
| `mq_notify01` | ipc | ipc | partial | `2/3` | no | LA 2/3; TBROK: Test killed by SIGSEGV! |
| `msgget02` | ipc | ipc | pass | `6/6` | yes | LA 6/6 |
| `semctl03` | ipc | ipc | partial | `6/8` | yes | LA 6/8; TCONF: EFAULT is skipped for libc variant |
| `semget02` | ipc | ipc | pass | `6/6` | yes | LA 6/6 |
| `kcmp02` | process | process | pass | `6/6` | yes | LA 6/6 |
| `alarm02` | time | time | pass | `6/6` | yes | LA 6/6 |
| `timerfd02` | time | time | pass | `6/6` | yes | LA 6/6 |
| `chown05` | vfs | vfs | partial | `6/12` | yes | LA 6/12; TFAIL: testfile: incorrect ownership set, expected 700 701 |
| `creat01` | vfs | vfs | pass | `6/6` | yes | LA 6/6 |
| `creat08` | vfs | vfs | fail | `0/1` | no | LA 0/1; TBROK: dir_a: Incorrect group, 0 != 1 |
| `fchmodat01` | vfs | vfs | pass | `6/6` | yes | LA 6/6 |
| `flock04` | vfs | vfs | pass | `6/6` | yes | LA 6/6 |
| `fstat02` | vfs | vfs | pass | `6/6` | yes | LA 6/6 |
| `fstat02_64` | vfs | vfs | pass | `6/6` | yes | LA 6/6 |
| `fstatat01` | vfs | vfs | pass | `6/6` | yes | LA 6/6 |
| `lchown01` | vfs | vfs | pass | `6/6` | yes | LA 6/6 |
| `open10` | vfs | vfs | fail | `0/1` | no | LA 0/1; TBROK: dir_a: Incorrect group, 0 != 1 |
| `readlinkat02` | vfs | vfs | partial | `5/6` | yes | LA 5/6; TFAIL: readlinkat(3, symlink_file, NULL, 0) succeeded |
| `madvise01` | vm | vm | partial | `6/20` | yes | LA 6/20; TFAIL: madvise test for MADV_REMOVE failed with return = -1, errno = 38 : ??? |
| `setregid01` | cred | cred | pass | `5/5` | yes | LA 5/5 |
| `setresgid01` | cred | cred | pass | `5/5` | yes | LA 5/5 |
| `epoll_wait03` | event | event | pass | `5/5` | yes | LA 5/5 |
| `epoll_wait07` | event | event | pass | `5/5` | yes | LA 5/5 |
| `eventfd02` | event | event | pass | `5/5` | yes | LA 5/5 |
| `pwrite02` | fd-io | fd-io | pass | `5/5` | yes | LA 5/5 |
| `pwrite02_64` | fd-io | fd-io | pass | `5/5` | yes | LA 5/5 |
| `sendfile04` | fd-io | fd-io | pass | `5/5` | yes | LA 5/5 |
| `sendfile04_64` | fd-io | fd-io | pass | `5/5` | yes | LA 5/5 |
| `sync_file_range01` | fd-io | fd-io | pass | `5/5` | yes | LA 5/5 |
| `mq_open01` | ipc | ipc | partial | `5/10` | yes | LA 5/10; TBROK: Failed to open FILE '/proc/sys/fs/mqueue/queues_max' for reading: ENOENT (2) |
| `shmctl08` | ipc | ipc | partial | `5/6` | yes | LA 5/6; TFAIL: shm_ctime not updated old 0 new 0 |
| `kcmp01` | process | process | pass | `5/5` | yes | LA 5/5 |
| `faccessat201` | vfs | vfs | partial | `5/7` | yes | LA 5/7; TFAIL: faccessat2(-1, /tmp/LTP_facCCIfDj/faccessat2dir/faccessat2file, R_OK, 0) failed: EBADF (9) |
| `fchmodat02` | vfs | vfs | partial | `5/6` | yes | LA 5/6; TFAIL: fchmodat() with invalid address expected EFAULT: ENAMETOOLONG (36) |
| `fchownat01` | vfs | vfs | pass | `5/5` | yes | LA 5/5 |
| `mkdirat01` | vfs | vfs | pass | `5/5` | yes | LA 5/5 |
| `mknod06` | vfs | vfs | partial | `5/6` | yes | LA 5/6; TFAIL: mknod06.c:161: mknod() fails, Invalid address, errno:36, expected errno:14 |
| `mknodat01` | vfs | vfs | pass | `5/5` | yes | LA 5/5 |
| `statx03` | vfs | vfs | partial | `5/7` | yes | LA 5/7; TFAIL: statx() should fail with EFAULT: ENAMETOOLONG (36) |
| `truncate03` | vfs | vfs | partial | `5/8` | yes | LA 5/8; TFAIL: truncate(tc->pathname, tc->length) succeeded |
| `truncate03_64` | vfs | vfs | partial | `5/8` | yes | LA 5/8; re-enabled after the 2026-06-04 interrupted rerun passed the previous long-run allocation-panic point |
| `unlink07` | vfs | vfs | partial | `5/6` | yes | LA 5/6; TFAIL: invalid address expected EFAULT: ENAMETOOLONG (36) |
| `setegid01` | cred | cred | pass | `4/4` | yes | LA 4/4 |
| `setresuid02` | cred | cred | pass | `4/4` | yes | LA 4/4 |
| `setreuid03` | cred | cred | partial | `4/14` | yes | LA 4/14; non-root setreuid cases unexpectedly succeed |
| `eventfd01` | event | event | pass | `4/4` | yes | LA 4/4 |
| `futex_wait01` | event | event | pass | `4/4` | yes | LA 4/4 |
| `select04` | event | event | partial | `4/7` | yes | LA 4/7; TCONF: syscall(-1) __NR_select not supported on your arch |
| `dup201` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `dup203` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `dup204` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl07` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl07_64` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl13` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl13_64` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl30` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl30_64` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `ioctl_ns07` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `lseek01` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `readv02` | fd-io | fd-io | partial | `4/5` | yes | LA 4/5; TFAIL: readv(3, 0x120038820, 1) succeeded |
| `sendfile03` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `sendfile03_64` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `msgrcv02` | ipc | ipc | partial | `4/8` | yes | LA 4/8; TFAIL: msgrcv(5, 0x1200391c0, -1, 2, 0) succeeded |
| `semctl09` | ipc | ipc | partial | `4/16` | yes | LA 4/16; TFAIL: SEM_INFO haven't returned a valid index: EINVAL (22) |
| `semop01` | ipc | ipc | pass | `4/4` | yes | LA 4/4 |
| `shmat01` | ipc | ipc | pass | `4/4` | yes | LA 4/4 |
| `get_robust_list01` | process | process | partial | `4/5` | yes | LA 4/5; TFAIL: get_robust_list01.c:172: get_robust_list failed unexpectedly: errno=ESRCH(3): No such process |
| `getpgid01` | process | process | partial | `4/8` | yes | LA 4/8; TFAIL: getpgid(16) failed: ESRCH (3) |
| `pidfd_send_signal02` | process | process | pass | `4/4` | yes | LA 4/4 |
| `sched_getaffinity01` | sched | sched | pass | `4/4` | yes | LA 4/4 |
| `sched_getattr02` | sched | sched | pass | `4/4` | yes | LA 4/4 |
| `sched_setaffinity01` | sched | sched | pass | `4/4` | yes | LA 4/4 |
| `sched_setattr01` | sched | sched | fail | `0/4` | no | LA 0/4 |
| `getrandom01` | smoke | random | pass | `4/4` | yes | LA 4/4 |
| `getrandom02` | smoke | random | pass | `4/4` | yes | LA 4/4 |
| `clock_nanosleep04` | time | time | pass | `4/4` | yes | LA 4/4 |
| `timerfd_settime01` | time | time | pass | `4/4` | yes | LA 4/4 |
| `flock06` | vfs | vfs | pass | `4/4` | yes | LA 4/4 |
| `lstat02` | vfs | vfs | partial | `5/6` | yes | LA 5/6; TFAIL: lstat() returned 0, expected -1: SUCCESS (0) |
| `lstat02_64` | vfs | vfs | partial | `5/6` | yes | LA 5/6; TFAIL: lstat() returned 0, expected -1: SUCCESS (0) |
| `stat03` | vfs | vfs | partial | `4/6` | yes | LA 4/6; TFAIL: stat(tc->pathname, &stat_buf) succeeded |
| `stat03_64` | vfs | vfs | partial | `4/6` | yes | LA 4/6; TFAIL: stat(tc->pathname, &stat_buf) succeeded |
| `statx02` | vfs | vfs | partial | `4/5` | yes | LA 4/5; TFAIL: Statx symlink flag failed to work as expected |
| `symlink03` | vfs | vfs | partial | `4/6` | yes | LA 4/6; TFAIL: symlink03.c:189: symlink() returned 0, expected -1, errno:13 |
| `mincore01` | vm | vm | pass | `4/4` | yes | LA 4/4 |
| `mlock01` | vm | vm | pass | `4/4` | yes | LA 4/4 |
| `mlock201` | vm | vm | partial | `4/8` | yes | LA 4/8; TFAIL: mlock2(0) locked 0 pages, expected 1 |
| `mlock202` | vm | vm | pass | `4/4` | yes | LA 4/4 |
| `munlock01` | vm | vm | pass | `4/4` | yes | LA 4/4 |
| `remap_file_pages02` | vm | vm | pass | `4/4` | yes | LA 4/4 |
| `capset01` | cred | cred | pass | `3/3` | yes | LA 3/3 |
| `setreuid04` | cred | cred | pass | `3/3` | yes | LA 3/3 |
| `epoll_ctl01` | event | event | pass | `3/3` | yes | LA 3/3 |
| `epoll_wait01` | event | event | pass | `3/3` | yes | LA 3/3 |
| `eventfd03` | event | event | pass | `3/3` | yes | LA 3/3 |
| `eventfd04` | event | event | pass | `3/3` | yes | LA 3/3 |
| `pselect02` | event | event | pass | `3/3` | yes | LA 3/3 |
| `pselect02_64` | event | event | pass | `3/3` | yes | LA 3/3 |
| `close01` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `dup07` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `dup3_02` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `fcntl29` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `fcntl29_64` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `pread02` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `pread02_64` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `preadv01` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `preadv01_64` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `pwritev01` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `pwritev01_64` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `read02` | fd-io | fd-io | partial | `3/5` | yes | LA 3/5; TCONF: O_DIRECT not supported on tmpfs filesystem |
| `write05` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `mq_unlink01` | ipc | ipc | partial | `3/4` | yes | LA 3/4; TFAIL: mq_unlink returned 0, expected -1, expected errno EACCES (13): SUCCESS (0) |
| `msgctl12` | ipc | ipc | partial | `3/4` | yes | LA 3/4; TFAIL: msgctl() test MSG_STAT failed with errno: 22 |
| `semctl05` | ipc | ipc | pass | `3/3` | yes | LA 3/3 |
| `semget01` | ipc | ipc | pass | `3/3` | yes | LA 3/3 |
| `shmat02` | ipc | ipc | pass | `3/3` | yes | LA 3/3 |
| `shmget04` | ipc | ipc | pass | `3/3` | yes | LA 3/3 |
| `setns01` | mount | mount | partial | `3/5` | yes | LA 3/5; TFAIL: without CAP_SYS_ADMIN ret=0 expected=-1 |
| `clone08` | process | process | partial | `1/4` | yes | LA 1/4; TBROK: CLONE_PARENT clone() failed: EINVAL (22) |
| `execve03` | process | process | partial | `3/6` | yes | LA 3/6; TFAIL: execve failed unexpectedly; expected Filename too long: ENOENT (2) |
| `pidfd_getfd02` | process | process | partial | `3/5` | yes | LA 3/5; first errno cases pass, checkpoint paths time out |
| `pidfd_open02` | process | process | pass | `3/3` | yes | LA 3/3 |
| `getrusage02` | sched | sched | partial | `3/4` | yes | LA 3/4; TCONF: EFAULT is skipped for libc variant |
| `membarrier01` | sched | sched | partial | `3/4` | yes | LA 3/4; TBROK: Test 3 haven't reported results! |
| `sigwait01` | signal | signal | partial | `3/4` | yes | LA 3/4; TBROK: kill(15,SIGTERM) failed: ESRCH (3) |
| `syscall01` | smoke | syscall | pass | `3/3` | yes | LA 3/3 |
| `ulimit01` | smoke | libc | pass | `3/3` | yes | LA 3/3 |
| `alarm05` | time | time | pass | `3/3` | yes | LA 3/3 |
| `getitimer02` | time | time | pass | `3/3` | yes | LA 3/3 |
| `nanosleep04` | time | time | pass | `3/3` | yes | LA 3/3 |
| `setitimer02` | time | time | pass | `3/3` | yes | LA 3/3 |
| `timer_gettime01` | time | time | pass | `3/3` | yes | LA 3/3 |
| `timerfd01` | time | time | partial | `4/12` | yes | LA 4/12; TFAIL: no ticks happened |
| `timerfd_gettime01` | time | time | pass | `3/3` | yes | LA 3/3 |
| `chmod03` | vfs | vfs | partial | `2/4` | yes | LA 2/4; TFAIL: stat(testfile) mode=100644 |
| `faccessat01` | vfs | vfs | pass | `3/3` | yes | LA 3/3 |
| `flock01` | vfs | vfs | pass | `3/3` | yes | LA 3/3 |
| `flock02` | vfs | vfs | pass | `3/3` | yes | LA 3/3 |
| `ftruncate03` | vfs | vfs | partial | `3/4` | yes | LA 3/4; TFAIL: ftruncate() succeeded unexpectedly and got 0 |
| `ftruncate03_64` | vfs | vfs | partial | `3/4` | yes | LA 3/4; TFAIL: ftruncate() succeeded unexpectedly and got 0 |
| `getcwd01` | vfs | vfs | partial | `3/5` | yes | LA 3/5; TFAIL: tst_syscall(__NR_getcwd, tc->buf, tc->size) expected ERANGE: EINVAL (22) |
| `lchown02` | vfs | vfs | partial | `3/6` | yes | LA 3/6; TFAIL: lchown02.c:154: lchown(2) returned 0, expected -1, errno:1 |
| `open12` | vfs | vfs | partial | `3/5` | yes | LA 3/5; TBROK: open12.c:224: write(3,0x1200233a8,11) failed: errno=EINVAL(22): Invalid argument |
| `mlock02` | vm | vm | pass | `3/3` | yes | LA 3/3 |
| `mlockall01` | vm | vm | pass | `3/3` | yes | LA 3/3 |
| `mlockall03` | vm | vm | pass | `3/3` | yes | LA 3/3 |
| `mmap09` | vm | vm | pass | `3/3` | yes | LA 3/3 |
| `mremap06` | vm | vm | pass | `3/3` | yes | LA 3/3 |
| `munmap03` | vm | vm | pass | `3/3` | yes | LA 3/3 |
| `setgid03` | cred | cred | pass | `2/2` | yes | LA 2/2 |
| `setresuid05` | cred | cred | pass | `2/2` | yes | LA 2/2 |
| `epoll_create01` | event | event | partial | `2/3` | yes | LA 2/3; TCONF: syscall(-1) __NR_epoll_create not supported on your arch |
| `epoll_create1_01` | event | event | pass | `2/2` | yes | LA 2/2 |
| `epoll_create1_02` | event | event | pass | `2/2` | yes | LA 2/2 |
| `eventfd05` | event | event | pass | `2/2` | yes | LA 2/2 |
| `eventfd2_01` | event | event | pass | `2/2` | yes | LA 2/2 |
| `eventfd2_02` | event | event | pass | `2/2` | yes | LA 2/2 |
| `eventfd2_03` | event | event | pass | `2/2` | yes | LA 2/2 |
| `futex_wait_bitset01` | event | event | pass | `2/2` | yes | LA 2/2 |
| `poll01` | event | event | pass | `2/2` | yes | LA 2/2 |
| `copy_file_range03` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `dup01` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `dup02` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `dup04` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `dup207` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `dup3_01` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `fallocate01` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl09` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl09_64` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl10` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl10_64` | fd-io | fd-io | pass | `4/4` | yes | LA 4/4 |
| `fcntl15_64` | fd-io | fd-io | partial | `2/3` | yes | LA 2/3; TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `fcntl15` | fd-io | fd-io | partial | `2/3` | yes | LA 2/3; TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `fcntl27` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `fcntl27_64` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `fsync03` | fd-io | fd-io | partial | `2/5` | yes | LA 2/5; TFAIL: fsync(): unexpected error: ENODEV (19) |
| `llseek02` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `lseek07` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `pipe03` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `sendfile02` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `sendfile02_64` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `write02` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `write06` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `sethostname01` | heavy | heavy | pass | `2/2` | yes | LA 2/2 |
| `uname01` | heavy | heavy | pass | `2/2` | yes | LA 2/2 |
| `msgctl03` | ipc | ipc | pass | `2/2` | yes | LA 2/2 |
| `msgctl06` | ipc | ipc | partial | `2/10` | yes | LA 2/10; TFAIL: MSG_INFO haven't returned a valid index: EINVAL (22) |
| `msgrcv01` | ipc | ipc | partial | `2/4` | yes | LA 2/4; TFAIL: PID of last msgrcv(2) mismatched |
| `semctl04` | ipc | ipc | pass | `2/2` | yes | LA 2/2 |
| `shmdt02` | ipc | ipc | pass | `2/2` | yes | LA 2/2 |
| `clone01` | process | process | pass | `2/2` | yes | LA 2/2 |
| `clone02` | process | process | pass | `2/2` | yes | LA 2/2 |
| `fork01` | process | process | pass | `2/2` | yes | LA 2/2 |
| `fork10` | process | process | pass | `2/2` | yes | LA 2/2 |
| `getpgid02` | process | process | pass | `2/2` | yes | LA 2/2 |
| `getpgrp01` | process | process | pass | `2/2` | yes | LA 2/2 |
| `getpid02` | process | process | pass | `2/2` | yes | LA 2/2 |
| `gettid01` | process | process | pass | `2/2` | yes | LA 2/2 |
| `setpgrp02` | process | process | pass | `2/2` | yes | LA 2/2 |
| `setsid01` | process | process | partial | `2/4` | yes | LA 2/4; TFAIL: setsid01.c:155: setpgid failed, errno :38 |
| `waitpid03` | process | process | pass | `2/2` | yes | LA 2/2 |
| `waitpid04` | process | process | partial | `2/4` | yes | LA 2/4; TFAIL: waipid(-1, NULL, 0xffffffff) expected EINVAL: ECHILD (10) |
| `getrlimit02` | sched | sched | pass | `2/2` | yes | LA 2/2 |
| `getrusage01` | sched | sched | pass | `2/2` | yes | LA 2/2 |
| `setrlimit01` | sched | sched | partial | `2/4` | yes | LA 2/4; TFAIL: setrlimit01.c:183: setrlimit failed, expected 10 got 26 |
| `kill02` | signal | signal | pass | `2/2` | yes | LA 2/2 |
| `rt_sigprocmask02` | signal | signal | pass | `2/2` | yes | LA 2/2 |
| `sigaltstack02` | signal | signal | pass | `2/2` | yes | LA 2/2 |
| `signalfd01` | signal | signal | pass | `2/2` | yes | LA 2/2 |
| `getrandom05` | smoke | random | pass | `2/2` | yes | LA 2/2 |
| `memcmp01` | smoke | string | pass | `2/2` | yes | LA 2/2 |
| `memcpy01` | smoke | string | pass | `2/2` | yes | LA 2/2 |
| `alarm03` | time | time | pass | `2/2` | yes | LA 2/2 |
| `alarm06` | time | time | pass | `2/2` | yes | LA 2/2 |
| `alarm07` | time | time | pass | `2/2` | yes | LA 2/2 |
| `gettimeofday01` | time | time | partial | `2/3` | yes | LA 2/3; TFAIL: tst_syscall(__NR_gettimeofday, tc->tv, tc->tz) succeeded |
| `nanosleep02` | time | time | pass | `2/2` | yes | LA 2/2 |
| `time01` | time | time | pass | `2/2` | yes | LA 2/2 |
| `timer_getoverrun01` | time | time | pass | `2/2` | yes | LA 2/2 |
| `timerfd_create01` | time | time | pass | `2/2` | yes | LA 2/2 |
| `chown02` | vfs | vfs | partial | `2/3` | yes | LA 2/3; TFAIL: testfile1: wrong mode permissions 0106770, expected 0100770 |
| `faccessat02` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `faccessat202` | vfs | vfs | partial | `2/6` | yes | LA 2/6; TFAIL: faccessat2() with invalid address expected EFAULT: ENAMETOOLONG (36) |
| `fstat03` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `fstat03_64` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `fstatfs02` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `fstatfs02_64` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `ftruncate01` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `ftruncate01_64` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `mknod02` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `open01` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `open08` | vfs | vfs | partial | `2/6` | yes | LA 2/6; TFAIL: O_RDWR succeeded |
| `open09` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `open13` | vfs | vfs | partial | `2/5` | yes | LA 2/5; TFAIL: open13.c:144: fchmod(2) succeeded unexpectedly |
| `openat02` | vfs | vfs | partial | `2/4` | yes | LA 2/4; TBROK: openat02.c:199: write(3,0x120023588,7) failed: errno=EINVAL(22): Invalid argument |
| `readlink01` | vfs | vfs | partial | `2/4` | yes | LA 2/4 |
| `readlink01A` | vfs | vfs | partial | `2/4` | yes | LA 2/4; TBROK: symlink01.c:986: lstat(2) Failure when accessing symbolic symbolic link file which should contain object path to (null) file |
| `stat02` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `stat02_64` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `symlink04` | vfs | vfs | partial | `2/4` | yes | LA 2/4; TBROK: lstat(slink_file,0x401ccb40) failed: ENOENT (2) |
| `truncate02` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `truncate02_64` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `unlink05` | vfs | vfs | pass | `2/2` | yes | LA 2/2 |
| `unlink08` | vfs | vfs | partial | `2/4` | yes | LA 2/4; TFAIL: unwritable directory succeeded |
| `madvise10` | vm | vm | partial | `2/6` | yes | LA 2/6; TFAIL: madvise(0x2000, 16384, 0x12): ENOSYS (38) |
| `mlock05` | vm | vm | pass | `2/2` | yes | LA 2/2 |
| `msync03` | vm | vm | partial | `2/6` | yes | LA 2/6; TFAIL: msync03.c:141: msync succeeded unexpectedly |
| `munlockall01` | vm | vm | pass | `2/2` | yes | LA 2/2 |
| `io_uring01` | aio | aio | partial | `1/2` | yes | LA 1/2; mmap SQ/CQ ring returns EINVAL, exits cleanly |
| `capset04` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getegid02` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getegid02_16` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `geteuid01` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `geteuid02` | cred | cred | partial | `1/2` | yes | LA 1/2; /proc/self/status conversion count is 0, exits cleanly |
| `getgid01` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getgid03` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getresgid01` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getresgid02` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getresgid03` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getresuid01` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getresuid02` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getresuid03` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getuid01` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `getuid03` | cred | cred | partial | `1/2` | yes | LA 1/2; /proc/self/status conversion count is 0, exits cleanly |
| `setgid01` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `setgroups02` | cred | cred | partial | `1/3` | yes | LA 1/3; getgroups returns ENOSYS and group value remains 0 |
| `setgroups03` | cred | cred | partial | `1/3` | yes | LA 1/3; invalid setgroups cases unexpectedly succeed |
| `setresgid04` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `setresuid04` | cred | cred | partial | `1/3` | yes | LA 1/3; non-root open permission checks are too permissive |
| `setreuid07` | cred | cred | partial | `1/3` | yes | LA 1/3; non-root open permission checks are too permissive |
| `setuid01` | cred | cred | pass | `1/1` | yes | LA 1/1 |
| `epoll_ctl04` | event | event | pass | `1/1` | yes | LA 1/1 |
| `epoll_ctl05` | event | event | pass | `1/1` | yes | LA 1/1 |
| `futex_cmp_requeue02` | event | event | pass | `3/3` | yes | LA 3/3 in 2026-06-03 musl/glibc focused submit-tail rerun |
| `futex_wait02` | event | event | pass | `1/1` | yes | LA 1/1 |
| `futex_wait03` | event | event | fail | `0/1` | no | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `futex_wait04` | event | event | pass | `1/1` | yes | LA 1/1 |
| `pselect01` | event | event | fail | `0/7` | no | LA 0/7; TFAIL: pselect() woken up early 468 times range: [999,882] |
| `pselect01_64` | event | event | fail | `0/7` | no | LA 0/7; TFAIL: pselect() woken up early 424 times range: [999,895] |
| `pselect03` | event | event | pass | `1/1` | yes | LA 1/1 |
| `pselect03_64` | event | event | pass | `1/1` | yes | LA 1/1 |
| `close02` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `dup03` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `dup05` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `dup06` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `dup205` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `dup206` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl01` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl01_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl03` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl03_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl04` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl04_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl08` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl08_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl12` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl12_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl16` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl16_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl18` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `fcntl18_64` | fd-io | fd-io | pass | `3/3` | yes | LA 3/3 |
| `fcntl22` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl22_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fcntl34` | fd-io | fd-io | fail | `0/1` | no | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `fcntl34_64` | fd-io | fd-io | fail | `0/1` | no | LA 0/1; TBROK: Test killed by SIGSEGV! |
| `fdatasync01` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `fsync02` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `llseek01` | fd-io | fd-io | partial | `1/2` | yes | LA 1/2; TFAIL: write successful after file size limit |
| `pipe01` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pipe04` | fd-io | fd-io | pass | `2/2` | yes | LA 2/2 |
| `pipe05` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pipe06` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pipe07` | fd-io | fd-io | partial | `1/2` | yes | LA 1/2; TFAIL: exp_num_pipes (1024) != num_pipe_fds (1020) |
| `pipe08` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pipe09` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pipe10` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pipe12` | fd-io | fd-io | partial | `1/2` | yes | LA 1/2; TBROK: ioctl(4,(0x541B),...) failed: ENOTTY (25) |
| `pipe14` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pread01` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pread01_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pwrite01` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pwrite01_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pwrite03` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pwrite03_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pwrite04` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `pwrite04_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `read01` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `read04` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `sendfile05` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `sendfile05_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `sendfile06` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `sendfile06_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `sendfile08` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `sendfile08_64` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `write01` | fd-io | fd-io | partial | `1/1` | no | LA musl passed, but 2026-06-03 LA glibc focused submit-tail rerun failed `0/1` with `EINVAL`; not active submit |
| `write03` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `writev02` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `writev05` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `writev06` | fd-io | fd-io | pass | `1/1` | yes | LA 1/1 |
| `getdomainname01` | heavy | heavy | pass | `1/1` | yes | LA 1/1 |
| `modify_ldt01` | heavy | heavy | pass | `1/1` | yes | LA 1/1 |
| `modify_ldt02` | heavy | heavy | pass | `1/1` | yes | LA 1/1 |
| `modify_ldt03` | heavy | heavy | pass | `1/1` | yes | LA 1/1 |
| `newuname01` | heavy | heavy | pass | `1/1` | yes | LA 1/1 |
| `ptrace05` | heavy | heavy | partial | `1/124` | yes | LA 1/124; TFAIL: ptrace05.c:96: Failed to ptrace(PTRACE_TRACEME, ...) properly: errno=ENOSYS(38): Function not implemented |
| `uname02` | heavy | heavy | pass | `1/1` | yes | LA 1/1 |
| `uname04` | heavy | heavy | pass | `2/2` | yes | LA 2/2 in 2026-06-03 musl/glibc focused submit-tail rerun |
| `msgctl02` | ipc | ipc | partial | `1/2` | yes | LA 1/2; TFAIL: msg_qbytes = 16384, expected 16383 |
| `msgget01` | ipc | ipc | pass | `1/1` | yes | LA 1/1 |
| `msgrcv08` | ipc | ipc | pass | `1/1` | yes | LA 1/1 |
| `msgsnd01` | ipc | ipc | partial | `1/3` | yes | LA 1/3; TFAIL: PID of last msgsnd(2) mismatched |
| `semctl02` | ipc | ipc | pass | `1/1` | yes | LA 1/1 |
| `semctl06` | ipc | ipc | pass | `1/1` | yes | LA 1/1 |
| `semop04` | ipc | ipc | pass | `1/1` | yes | LA 1/1 |
| `semop05` | ipc | ipc | fail | `0/1` | no | LA 0/1 |
| `shmat04` | ipc | ipc | pass | `1/1` | yes | LA 1/1 |
| `shmctl07` | ipc | ipc | partial | `1/4` | yes | LA 1/4; TFAIL: shmctl(12, SHM_LOCK, NULL): EINVAL (22) |
| `shmdt01` | ipc | ipc | partial | `1/2` | yes | LA 1/2; TBROK: Test killed by SIGSEGV! |
| `unshare02` | mount | mount | partial | `1/2` | yes | LA 1/2; TFAIL: unshare(CLONE_NEWNS) expected EPERM: EINVAL (22) |
| `clone03` | process | process | pass | `1/1` | yes | LA 1/1 |
| `clone05` | process | process | pass | `1/1` | yes | LA 1/1 |
| `clone06` | process | process | pass | `1/1` | yes | LA 1/1 |
| `clone07` | process | process | pass | `1/1` | yes | LA 1/1 |
| `clone302` | process | process | partial | `1/2` | yes | LA 1/2; TCONF: syscall(435) __NR_clone3 not supported on your arch |
| `execl01` | process | process | audit-only | `1/1` | no | LA/RV musl focused runs pass, but RV glibc candidate sweep failed; not promoted on 2026-06-04 |
| `execle01` | process | process | audit-only | `1/1` | no | LA/RV musl focused runs pass, but RV glibc candidate sweep failed; not promoted on 2026-06-04 |
| `execlp01` | process | process | audit-only | `1/1` | no | LA/RV musl focused runs pass, but RV glibc candidate sweep failed; not promoted on 2026-06-04 |
| `execv01` | process | process | audit-only | `1/1` | no | LA/RV musl focused runs pass, but RV glibc candidate sweep failed; not promoted on 2026-06-04 |
| `execve01` | process | process | audit-only | `1/1` | no | LA/RV musl focused runs pass, but RV glibc candidate sweep failed; not promoted on 2026-06-04 |
| `execve06` | process | process | audit-only | `1/1` | no | LA/RV musl focused runs pass, but RV glibc candidate sweep failed; not promoted on 2026-06-04 |
| `execvp01` | process | process | audit-only | `1/1` | no | LA/RV musl focused runs pass, but RV glibc candidate sweep failed; not promoted on 2026-06-04 |
| `exit01` | process | process | audit-only | `1/1` | no | Old-format row scores as `0/0` in focused judge output; not promoted |
| `exit02` | process | process | pass | `1/1` | yes | LA 1/1 |
| `exit_group01` | process | process | audit-only | `1/1` | no | LA/RV musl focused runs pass, but 2026-06-04 RV full submit timed out at glibc `exit_group01` and ended with `trap-action-terminate`; not active submit |
| `fork03` | process | process | pass | `1/1` | yes | LA 1/1 |
| `fork04` | process | process | partial | `1/2` | yes | LA 1/2; TBROK: tst_checkpoint_wait(0, 10000) failed: ETIMEDOUT (110) |
| `fork07` | process | process | pass | `1/1` | yes | LA 1/1 |
| `fork08` | process | process | pass | `1/1` | yes | LA 1/1 |
| `fork09` | process | process | pass | `1/1` | yes | LA 1/1 |
| `getppid01` | process | process | pass | `1/1` | yes | LA 1/1 |
| `getppid02` | process | process | pass | `1/1` | yes | LA 1/1 |
| `getsid01` | process | process | pass | `1/1` | yes | LA 1/1 |
| `getsid02` | process | process | pass | `1/1` | yes | LA 1/1 |
| `personality02` | process | process | pass | `1/1` | yes | LA 1/1 |
| `pidfd_getfd01` | process | process | partial | `1/3` | yes | LA 1/3; fd duplication passes, checkpoint cleanup times out |
| `pidfd_open01` | process | process | pass | `1/1` | yes | LA 1/1 |
| `pidfd_open04` | process | process | partial | `1/4` | yes | LA 1/4; `PIDFD_NONBLOCK` passes, `waitid(P_PIDFD)` still returns ENOSYS and checkpoint cleanup times out |
| `set_robust_list01` | process | process | partial | `1/2` | yes | LA 1/2; TFAIL: set_robust_list01.c:117: set_robust_list: retval = 0 (expected -1), errno = 0 (expected 22) |
| `set_tid_address01` | process | process | pass | `1/1` | yes | LA 1/1 |
| `setpgid01` | process | process | partial | `1/2` | yes | LA 1/2; TFAIL: setpgid01.c:87: test setpgid(9, 1) fail: TEST_ERRNO=ENOSYS(38): Function not implemented |
| `setpgrp01` | process | process | pass | `1/1` | yes | LA 1/1 |
| `vfork01` | process | process | pass | `1/1` | yes | LA 1/1 |
| `wait01` | process | process | pass | `1/1` | yes | LA 1/1 |
| `wait02` | process | process | pass | `1/1` | yes | LA 1/1 |
| `wait402` | process | process | pass | `1/1` | yes | LA 1/1 |
| `waitid04` | process | process | partial | `1/4` | yes | LA 1/4; checkpoint wait/wake time out, exits cleanly |
| `waitid05` | process | process | partial | `1/6` | yes | LA 1/6; TFAIL: waitid(P_PGID, pid_group+1, infop, WEXITED) expected ECHILD: ENOSYS (38) |
| `waitid06` | process | process | partial | `1/6` | yes | LA 1/6; TFAIL: waitid(P_PID, pid_child+1, infop, WEXITED) expected ECHILD: ENOSYS (38) |
| `sched_getattr01` | sched | sched | pass | `1/1` | yes | LA 1/1 |
| `setrlimit02` | sched | sched | partial | `1/2` | yes | LA 1/2; TFAIL: call succeeded unexpectedly |
| `setrlimit03` | sched | sched | partial | `1/2` | yes | LA 1/2; TFAIL: call succeeded unexpectedly (nr_open=1048576 rlim_cur=1024 rlim_max=1048577) |
| `setrlimit04` | sched | sched | pass | `1/1` | yes | LA 1/1 |
| `setrlimit05` | sched | sched | pass | `1/1` | yes | LA 1/1 |
| `kill06` | signal | signal | pass | `1/1` | yes | LA 1/1 |
| `kill07` | signal | signal | pass | `1/1` | yes | LA 1/1 |
| `kill08` | signal | signal | pass | `1/1` | yes | LA 1/1 |
| `kill09` | signal | signal | pass | `1/1` | yes | LA 1/1 |
| `kill12` | signal | signal | pass | `1/1` | yes | LA 1/1 |
| `sigaction01` | signal | signal | partial | `1/2` | yes | LA 1/2; TFAIL: sigaction01.c:125: SA_RESETHAND should not cause SA_SIGINFO to be cleared, but it was. |
| `sigaction02` | signal | signal | pass | `3/3` | yes | LA 3/3 |
| `sigaltstack01` | signal | signal | pass | `1/1` | yes | LA 1/1 |
| `signal02` | signal | signal | pass | `3/3` | yes | LA 3/3 |
| `signalfd4_01` | signal | signal | pass | `1/1` | yes | LA 1/1 |
| `signalfd4_02` | signal | signal | pass | `1/1` | yes | LA 1/1 |
| `gethostname01` | smoke | libc | pass | `1/1` | yes | LA 1/1 |
| `getpagesize01` | smoke | libc | pass | `1/1` | yes | LA 1/1 |
| `getrandom04` | smoke | random | pass | `1/1` | yes | LA 1/1 |
| `memset01` | smoke | string | pass | `1/1` | yes | LA 1/1 |
| `nftw01` | smoke | fs | partial | `1/2` | yes | LA 1/2; TFAIL: tools.c:267: Test failed |
| `nftw6401` | smoke | fs | partial | `1/2` | yes | LA 1/2; TFAIL: tools64.c:267: Test failed |
| `pathconf02` | smoke | fs | partial | `1/6` | yes | LA 1/6; TFAIL: pathconf() fail with path prefix is not a directory invalid retval 8: SUCCESS (0) |
| `string01` | smoke | string | pass | `1/1` | yes | LA 1/1 |
| `gettimeofday02` | time | time | pass | `1/1` | yes | LA 1/1 |
| `settimeofday02` | time | time | partial | `1/3` | yes | LA 1/3; TFAIL: settimeofday(&tc->tv, NULL) expected EINVAL: ENOSYS (38) |
| `timer_delete02` | time | time | pass | `1/1` | yes | LA 1/1 |
| `timer_settime03` | time | time | pass | `1/1` | yes | LA 1/1 |
| `times01` | time | time | pass | `1/1` | yes | LA 1/1 |
| `chdir04` | vfs | vfs | partial | `1/3` | yes | LA 1/3; TFAIL: chdir() expected ENAMETOOLONG: ENOENT (2) |
| `chmod05` | vfs | vfs | fail | `0/1` | no | LA 0/1; TFAIL: testdir: Incorrect modes 040755, Expected 041777 |
| `chmod07` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `chown01` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `chown03` | vfs | vfs | partial | `1/2` | yes | LA 1/2; TFAIL: chown03_testfile: wrong mode permissions 0106770, expected 0100770 |
| `creat03` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `creat05` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `fchdir01` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `fchdir02` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `fchmod02` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `fchmod03` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `fchmod04` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `fchmod05` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `flock03` | vfs | vfs | partial | `1/3` | yes | LA 1/3; checkpoint wait/wake time out, exits cleanly |
| `getcwd03` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `link02` | vfs | vfs | partial | `1/2` | yes | LA 1/2; TFAIL: link(oldpath,newpath) returned 0 but stat link counts do not match 1 1 |
| `lstat01A` | vfs | vfs | partial | `1/3` | yes | LA 1/3; symlink01 alias lstat symbolic-link cases TBROK |
| `lstat01A_64` | vfs | vfs | partial | `1/3` | yes | LA 1/3; symlink01 alias lstat symbolic-link cases TBROK |
| `mkdir05` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `mknod05` | vfs | vfs | fail | `0/2` | no | LA 0/2; setgid bit not reflected in created directory mode |
| `mknod08` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `mknod09` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `open02` | vfs | vfs | partial | `1/2` | yes | LA 1/2; TFAIL: open() unprivileged O_RDONLY / O_NOATIME succeeded |
| `open03` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `open04` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `open07` | vfs | vfs | partial | `1/5` | yes | LA 1/5; TFAIL: open(O_NOFOLLOW) a symlink to file succeeded |
| `readdir01` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `rmdir01` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `statfs02` | vfs | vfs | partial | `1/6` | yes | LA 1/6; TFAIL: statfs() succeeded |
| `statfs02_64` | vfs | vfs | partial | `1/6` | yes | LA 1/6; TFAIL: statfs() succeeded |
| `symlink01` | vfs | vfs | partial | `1/5` | yes | LA 1/5; TBROK: symlink01.c:983: lstat(2) Failure when accessing symbolic symbolic link file which should contain %bc+eFhi!k path to (null) file |
| `symlink02` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `umask01` | vfs | vfs | pass | `1/1` | yes | LA 1/1 |
| `brk01` | vm | vm | partial | `1/2` | yes | LA 1/2; TCONF: brk() not implemented |
| `brk02` | vm | vm | partial | `1/2` | yes | LA 1/2; TCONF: brk() not implemented |
| `madvise02` | vm | vm | partial | `1/13` | yes | LA 1/13; TFAIL: MADV_NORMAL failed unexpectedly; expected - 22 : ???: ENOSYS (38) |
| `madvise05` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mincore02` | vm | vm | partial | `1/2` | yes | LA 1/2; TFAIL: locked_pages (0) != NUM_PAGES (4) |
| `mincore03` | vm | vm | partial | `1/2` | yes | LA 1/2; TFAIL: mincore reports resident pages as 0, but expected 3 |
| `mlock03` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mlock04` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mlock203` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mlockall02` | vm | vm | pass | `3/3` | yes | LA 3/3 |
| `mmap01` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mmap02` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mmap08` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mmap15` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mmap17` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mmap19` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mmap20` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mprotect01` | vm | vm | partial | `1/4` | yes | LA 1/4; TBROK: mprotect01.c:150: mmap failed |
| `mprotect03` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mprotect05` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mremap02` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mremap03` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `mremap04` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `msync01` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `msync02` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `munlock02` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `sbrk01` | vm | vm | partial | `1/3` | yes | LA 1/3; TFAIL: sbrk(8192) failed: ENOMEM (12) |
| `sbrk02` | vm | vm | pass | `1/1` | yes | LA 1/1 |
| `socket01` | network | socket | pass | `9/9` | no | Audit-only network row; not appended to LA submit. |
| `socket02` | network | socket | pass | `4/4` | no | Audit-only network row; not appended to LA submit. |
| `listen01` | network | socket | pass | `3/3` | no | Audit-only network row; not appended to LA submit. |
| `getsockname01` | network | socket | pass | `6/6` | no | Audit-only network row; not appended to LA submit. |
| `getsockopt01` | network | socket | pass | `9/9` | no | Audit-only network row; not appended to LA submit. |
| `getsockopt02` | network | socket | pass | `1/1` | no | Audit-only; LA combined submit reproduced `EADDRINUSE`/`trap-action-terminate`. |
| `setsockopt01` | network | socket | pass | `8/8` | no | Audit-only network row; not appended to LA submit. |
| `send01` | network | socket | pass | `6/6` | no | Audit-only network row; not appended to LA submit. |
| `send02` | network | socket | pass | `4/4` | no | Audit-only network row; not appended to LA submit. |
| `sendto01` | network | socket | pass | `10/10` | no | Audit-only network row; not appended to LA submit. |
| `sendto02` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
| `sendto03` | network | socket | pass | `2/2` | no | Audit-only network row; not appended to LA submit. |
| `recv01` | network | socket | pass | `5/5` | no | Audit-only network row; not appended to LA submit. |
| `recvfrom01` | network | socket | pass | `7/7` | no | Audit-only network row; not appended to LA submit. |
| `recvmsg01` | network | socket | pass | `10/10` | no | Audit-only network row; not appended to LA submit. |
| `recvmsg02` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
| `recvmsg03` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
| `sendmmsg01` | network | socket | pass | `4/4` | no | Audit-only network row; not appended to LA submit. |
| `sendmmsg02` | network | socket | pass | `4/4` | no | Audit-only network row; not appended to LA submit. |
| `recvmmsg01` | network | socket | partial | `1/2` | no | Audit-only; musl wrapper SIGSEGV after first EBADF subcase. |
| `bind01` | network | socket | pass | `7/7` | no | Audit-only network row; not appended to LA submit. |
| `bind02` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
| `bind03` | network | socket | pass | `3/3` | no | Audit-only; combined glibc runs hit `EADDRINUSE`/`address is in use`. |
| `bind04` | network | socket | timeout/stall | `-` | no | Audit-only; broad b4 run stalled after first TPASS. |
| `bind05` | network | socket | timeout/stall | `-` | no | Audit-only; single run hit 120s timeout after first TPASS/no summary. |
| `bind06` | network | socket | timeout/stall | `-` | no | Audit-only; single run hit 600s timeout before summary. |
| `connect01` | network | socket | pass | `7/7` | no | Audit-only network row; not appended to LA submit. |
| `connect02` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
| `accept01` | network | socket | pass | `5/5` | no | Audit-only network row; not appended to LA submit. |
| `accept02` | network | socket | timeout/stall | `-` | no | Audit-only; single run hit 120s timeout after first TPASS/no summary. |
| `accept03` | network | socket | pass | `23/23` | no | Audit-only network row; not appended to LA submit. |
| `accept4_01` | network | socket | partial | `8/9` | no | Audit-only; legacy socketcall accept4 variant unavailable. |
| `getpeername01` | network | socket | pass | `7/7` | no | Audit-only network row; not appended to LA submit. |
| `socketpair01` | network | socket | pass | `10/10` | no | Audit-only network row; not appended to LA submit. |
| `socketpair02` | network | socket | pass | `4/4` | no | Audit-only network row; not appended to LA submit. |
| `setsockopt02` | network | socket | pass | `2/2` | no | Audit-only network row; not appended to LA submit. |
| `setsockopt03` | network | socket | partial | `1/2` | no | Audit-only; 32-bit compat-only subcase is TCONF. |
| `setsockopt04` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
| `setsockopt06` | network | socket | timeout/stall | `-` | no | Audit-only; broad b6 run hit outer 300s timeout during this case. |
| `setsockopt08` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
| `setsockopt09` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
| `setsockopt10` | network | socket | pass | `1/1` | no | Audit-only network row; not appended to LA submit. |
