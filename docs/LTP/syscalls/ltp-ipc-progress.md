# LTP ipc Progress

`ipc` batch local tracking. Cases are from `tools/ltp-batches.py --batch ipc`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 67 | from `make ltp-batch-cases LTP_BATCH=ipc` |
| latest local run | focused high-score rerun | 2026-06-03 `semop02` RV/LA musl+glibc |
| cumulative scored | `247/343` | recorded rows in this document |
| reached case | `shmget06` | batch completed |
| logs | `target/oscomp/ltp-progress/ipc`, `target/oscomp/ltp-timeout-triage/ipc` | per-group stdout, single-case timeout triage logs, and serial snapshots |

## 2026-05-26 failure notes

- TFAIL: 19 recorded case(s); see per-case notes below.
- TCONF: 15 recorded case(s); see per-case notes below.
- TBROK: 6 recorded case(s); see per-case notes below.
- host timeout before case completed: 5 recorded case(s); see per-case notes below.
- EINVAL observed: 4 recorded case(s); see per-case notes below.
- panicked at: 1 recorded case(s); see per-case notes below.

## 2026-06-03 focused high-score rerun

`semop02` 复测结果：

- LA musl: `21/26`
- LA glibc: `21/26`
- RV musl: `21/26`
- RV glibc: `21/26`

修正点是 `nsops > SEMOPM` 返回 `E2BIG`，不再返回 `EINVAL`。剩下两处
失败是 `semop` / `semtimedop` 变体仍然“unexpectedly succeeded”，后续需要继续
补 System V semaphore 的错误路径。对应日志：
`target/oscomp/ltp-highfix-open11-semop02-la-musl-20260603.txt`,
`target/oscomp/ltp-highfix-open11-semop02-la-glibc-20260603.txt`,
`target/oscomp/ltp-highfix-open11-semop02-rv-musl-20260603.txt`, and
`target/oscomp/ltp-highfix-open11-semop02-rv-glibc-20260603.txt`.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `mq_notify01` | 6/7 | partial | TFAIL: mq_notify failed unexpectedly, expected SUCCESS: EINVAL (22) |
| `mq_notify02` | 0/2 | fail | TFAIL: mq_notify(0, &(test->sevp)) expected EINVAL: EBADF (9) |
| `mq_notify03` | 1/2 | partial | TBROK: Test killed by SIGSEGV! |
| `mq_open01` | 5/10 | partial | TBROK: Failed to open FILE '/proc/sys/fs/mqueue/queues_max' for reading: ENOENT (2) |
| `mq_timedreceive01` | 24/30 | partial | TFAIL: mq_timedreceive() failed unexpectedly, expected EINVAL: EAGAIN/EWOULDBLOCK (11) |
| `mq_timedsend01` | 28/34 | partial | TFAIL: mq_timedsend() failed unexpectedly, expected EINVAL: EAGAIN/EWOULDBLOCK (11) |
| `mq_unlink01` | 3/4 | partial | TFAIL: mq_unlink returned 0, expected -1, expected errno EACCES (13): SUCCESS (0) |
| `msgctl01` | 13/14 | partial | TFAIL: msg_ctime = 0, expected 1779494416 |
| `msgctl02` | 1/2 | partial | TFAIL: msg_qbytes = 16384, expected 16383 |
| `msgctl03` | 2/2 | pass | EINVAL observed |
| `msgctl04` | 12/14 | partial | TCONF: EFAULT is skipped for libc variant |
| `msgctl05` | 0/1 | skip | TCONF: test requires struct msqid64_ds to have the time_high fields |
| `msgctl06` | 2/10 | partial | TFAIL: MSG_INFO haven't returned a valid index: EINVAL (22) |
| `msgctl12` | 3/4 | partial | TFAIL: msgctl() test MSG_STAT failed with errno: 22 |
| `msgget01` | 1/1 | pass |  |
| `msgget02` | 6/6 | pass |  |
| `msgget03` | 0/1 | skip | TCONF: Path not found: /proc/sys/kernel/msgmni: ENOENT (2) |
| `msgget04` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `msgget05` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `msgrcv01` | 2/4 | partial | TFAIL: PID of last msgrcv(2) mismatched |
| `msgrcv02` | 4/8 | partial | TFAIL: msgrcv(5, 0xe62280, -1, 2, 0) succeeded |
| `msgrcv03` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `msgrcv05` | 0/0 | hang | single-case rerun still host-times out after `msgrcv(..., 0)` returns `EAGAIN` instead of expected `EINTR` |
| `msgrcv06` | 0/0 | hang | single-case rerun still host-times out after `msgrcv(..., 0)` returns `EAGAIN` instead of expected `EIDRM` |
| `msgrcv07` | 11/13 | partial | single-case rerun exits cleanly; `MSG_EXCEPT`/`MSG_COPY` cases mismatch |
| `msgrcv08` | 1/1 | pass |  |
| `msgsnd01` | 1/3 | partial | TFAIL: PID of last msgsnd(2) mismatched |
| `msgsnd02` | 0/0 | panic | single-case rerun panics after five TPASS lines: `capacity overflow` in alloc raw_vec |
| `msgsnd05` | 0/0 | hang | single-case rerun still host-times out after `msgsnd(..., 0)` returns `EAGAIN` instead of expected `EINTR` |
| `msgsnd06` | 0/0 | hang | single-case rerun still host-times out after `msgsnd(..., 0)` returns `EAGAIN` instead of expected `EIDRM` |
| `msgstress01` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/msgmni' for reading: ENOENT (2) |
| `semctl01` | 8/12 | partial | TBROK: semctl(0, 0, 18,...) failed: EINVAL (22) |
| `semctl02` | 1/1 | pass |  |
| `semctl03` | 6/8 | partial | TCONF: EFAULT is skipped for libc variant |
| `semctl04` | 2/2 | pass |  |
| `semctl05` | 3/3 | pass |  |
| `semctl06` | 1/1 | pass |  |
| `semctl07` | 16/16 | pass |  |
| `semctl08` | 0/1 | skip | TCONF: test requires struct semid64_ds to have the time_high fields |
| `semctl09` | 4/16 | partial | TFAIL: SEM_INFO haven't returned a valid index: EINVAL (22) |
| `semget01` | 3/3 | pass |  |
| `semget02` | 6/6 | pass | EINVAL observed |
| `semget05` | 0/1 | skip | TCONF: Path not found: /proc/sys/kernel/sem: ENOENT (2) |
| `semop01` | 4/4 | pass |  |
| `semop02` | 21/26 | partial | `nsops > SEMOPM` now returns E2BIG; two semop/semtimedop variants still succeed unexpectedly |
| `semop03` | 8/8 | pass |  |
| `semop04` | 1/1 | pass |  |
| `semop05` | 1/1 | pass |  |
| `shmat01` | 4/4 | pass |  |
| `shmat02` | 3/3 | pass | EINVAL observed |
| `shmat03` | 0/1 | fail | TFAIL: We have mapped a VM address within the first 64Kb |
| `shmat04` | 1/1 | pass |  |
| `shmctl01` | 0/0 | hang | single-case rerun reaches child attach phase then host-times out; before hang, `IPC_STAT`/`SHM_STAT` report `shm_cpid=0` and `shm_ctime=0` |
| `shmctl02` | 16/22 | partial | single-case rerun exits cleanly; `SHM_LOCK`/`SHM_UNLOCK` permission cases return `EINVAL`, expected `EPERM` |
| `shmctl03` | 0/1 | fail | single-case rerun exits cleanly; `IPC_INFO` returns `EPERM`, expected success |
| `shmctl04` | 0/1 | skip | TCONF: kernel doesn't support SHM_STAT_ANY |
| `shmctl05` | 0/1 | skip | TCONF: syscall(234) __NR_remap_file_pages not supported on your arch |
| `shmctl06` | 0/1 | skip | TCONF: test requires struct shmid64_ds to have the time_high fields |
| `shmctl07` | 1/4 | partial | TFAIL: shmctl(2, SHM_LOCK, NULL): EINVAL (22) |
| `shmctl08` | 5/6 | partial | TFAIL: shm_ctime not updated old 0 new 0 |
| `shmdt01` | 1/2 | partial | TBROK: Test killed by SIGSEGV! |
| `shmdt02` | 2/2 | pass | EINVAL observed |
| `shmget02` | 0/1 | skip | TCONF: Path not found: /proc/sys/kernel/shmmax: ENOENT (2) |
| `shmget03` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/shmmni' for reading: ENOENT (2) |
| `shmget04` | 3/3 | pass |  |
| `shmget05` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `shmget06` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
