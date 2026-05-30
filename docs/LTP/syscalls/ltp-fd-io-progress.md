# LTP fd-io Progress

This file records the `fd-io` batch at case granularity. It is for local debugging and resume planning, not a single official full-batch report.

Scoring note: local scoring follows the official `judge_ltp-musl.py` where possible. Cases with `0/0` are kept as unknown unless the serial log has concrete detail points.

## Current Score

| Scope | Score | Note |
| --- | ---: | --- |
| fd-io accumulated local score | `852/1208` | Stitched from segmented runs; not a single official full-batch run |
| 2026-05-29 full-image partial run | `112/1326` | 300s direct QEMU run from `target/oscomp/testdata`; completed 54/55 started cases, timed out in `fcntl14_64`; serial `target/oscomp/os_serial_out_ltp_fdio_partial_20260529_205057.txt` |

Recorded through the end of the current fd-io case list. Latest changes included targeted positioned-I/O fixes, the conservative `splice` fix that raised `splice07` to `217/377`, legacy LTP scoring fallback for old no-summary cases, `fallocate` support, validation-only `readahead`/`sync_file_range`, `sendfile03/04/05` errno validation, refreshed `posix_fadvise02/04` scores, a `/bin/cat` shim that unblocks `posix_fadvise01/03`, page-backed `O_APPEND` handling for write/pwrite, basic page-backed `copy_file_range`, and `preadv2/pwritev2` support.

Timeout triage on 2026-05-26 ran the broad-batch skipped cases as single cases. `fcntl15`/`fcntl15_64` and `pipe02` now fail cleanly with checkpoint timeouts; `fcntl36`/`fcntl36_64` pass as single cases and did not reproduce the broad-batch OFD lock hang.

2026-05-29 direct full-image run command:

```bash
timeout 300s cargo xtask oscomp qemu --target rv64-qemu \
  --data target/oscomp/testdata --submit target/oscomp/submit \
  --suite ltp-batch:fd-io
python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata
```

The partial run confirms the early fd/dup/fallocate happy paths still execute,
but the first large score cliff is POSIX record locking: `fcntl11`,
`fcntl11_64`, `fcntl14`, and the in-progress `fcntl14_64` repeatedly return
`ENOSYS` for lock operations or publish wrong lock metadata. Device-backed setup
cases (`close_range01`, `copy_file_range01/02`, `fallocate04/05/06`) still fail
as `TBROK` while trying to create/acquire `test_dev.img`.

## Case Table

| Case | Score | State | Note |
| --- | ---: | --- | --- |
| `close01` | 3/3 | pass |  |
| `close02` | 1/1 | pass |  |
| `close_range01` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `close_range02` | 0/1 | fail | close_range unsupported/TCONF |
| `copy_file_range01` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `copy_file_range02` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `copy_file_range03` | 2/2 | pass | page-backed copy_file_range and mtime override fixed |
| `dup01` | 2/2 | pass |  |
| `dup02` | 2/2 | pass |  |
| `dup03` | 1/1 | pass |  |
| `dup04` | 2/2 | pass |  |
| `dup05` | 1/1 | pass |  |
| `dup06` | 1/1 | pass |  |
| `dup07` | 3/3 | pass |  |
| `dup201` | 4/4 | pass |  |
| `dup202` | 6/6 | pass |  |
| `dup203` | 4/4 | pass |  |
| `dup204` | 4/4 | pass |  |
| `dup205` | 1/1 | pass |  |
| `dup206` | 1/1 | pass |  |
| `dup207` | 2/2 | pass |  |
| `dup3_01` | 2/2 | pass |  |
| `dup3_02` | 3/3 | pass |  |
| `fallocate01` | 2/2 | pass | fallocate implemented |
| `fallocate02` | 8/8 | pass | fallocate errno validation fixed |
| `fallocate03` | 8/8 | pass | fallocate implemented |
| `fallocate04` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `fallocate05` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `fallocate06` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `fcntl01` | 1/1 | pass | legacy no-summary case; return code 0 |
| `fcntl01_64` | 1/1 | pass | legacy no-summary case; return code 0 |
| `fcntl02` | 6/6 | pass |  |
| `fcntl02_64` | 6/6 | pass |  |
| `fcntl03` | 1/1 | pass |  |
| `fcntl03_64` | 1/1 | pass |  |
| `fcntl04` | 1/1 | pass |  |
| `fcntl04_64` | 1/1 | pass |  |
| `fcntl05` | 6/6 | pass |  |
| `fcntl05_64` | 6/6 | pass |  |
| `fcntl07` | 4/4 | pass | legacy no-summary case; return code 0 |
| `fcntl07_64` | 4/4 | pass | legacy no-summary case; return code 0 |
| `fcntl08` | 1/1 | pass |  |
| `fcntl08_64` | 1/1 | pass |  |
| `fcntl09` | 2/2 | pass | legacy no-summary case; return code 0 |
| `fcntl09_64` | 2/2 | pass | legacy no-summary case; return code 0 |
| `fcntl10` | 2/2 | pass | legacy no-summary case; return code 0 |
| `fcntl10_64` | 2/2 | pass | legacy no-summary case; return code 0 |
| `fcntl11` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl11_64` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl12` | 1/1 | pass |  |
| `fcntl12_64` | 1/1 | pass |  |
| `fcntl13` | 4/4 | pass |  |
| `fcntl13_64` | 4/4 | pass |  |
| `fcntl14` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl14_64` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl15_64` | 2/3 | partial | single-case timeout triage: lock-conflict checks pass, then `tst_checkpoint_wait(0,10000)` returns ETIMEDOUT; exits cleanly |
| `fcntl15` | 2/3 | partial | single-case timeout triage: lock-conflict checks pass, then `tst_checkpoint_wait(0,10000)` returns ETIMEDOUT; exits cleanly |
| `fcntl16` | 1/1 | pass | legacy no-summary case; return code 0 |
| `fcntl16_64` | 1/1 | pass | legacy no-summary case; return code 0 |
| `fcntl17` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl17_64` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl18` | 1/1 | pass | legacy no-summary case; user-segv path but runner returned 0 |
| `fcntl18_64` | 1/1 | pass | legacy no-summary case; user-segv path but runner returned 0 |
| `fcntl19` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl19_64` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl20` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl20_64` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl21` | 0/1 | fail | legacy no-summary record-locking test; return code 1 |
| `fcntl21_64` | 0/1 | fail | legacy no-summary record-locking test; return code 1 |
| `fcntl22` | 1/1 | pass | legacy no-summary case; return code 0 |
| `fcntl22_64` | 1/1 | pass | legacy no-summary case; return code 0 |
| `fcntl23` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl23_64` | 0/1 | fail | legacy no-summary case; return code 1 |
| `fcntl24` | 0/1 | fail | legacy no-summary case; return code 32 |
| `fcntl24_64` | 0/1 | fail | legacy no-summary case; return code 32 |
| `fcntl25` | 0/1 | fail | legacy no-summary case; return code 32 |
| `fcntl25_64` | 0/1 | fail | legacy no-summary case; return code 32 |
| `fcntl26` | 0/1 | fail | legacy no-summary case; return code 32 |
| `fcntl26_64` | 0/1 | fail | legacy no-summary case; return code 32 |
| `fcntl27` | 2/2 | pass |  |
| `fcntl27_64` | 2/2 | pass |  |
| `fcntl29` | 3/3 | pass |  |
| `fcntl29_64` | 3/3 | pass |  |
| `fcntl30` | 4/4 | pass |  |
| `fcntl30_64` | 4/4 | pass |  |
| `fcntl31` | 0/5 | fail | legacy no-summary F_SETOWN/F_SETOWN_EX case; return code 1 |
| `fcntl31_64` | 0/5 | fail | legacy no-summary F_SETOWN/F_SETOWN_EX case; return code 1 |
| `fcntl32` | 0/9 | fail | legacy no-summary case; return code 32 |
| `fcntl32_64` | 0/9 | fail | legacy no-summary case; return code 32 |
| `fcntl33` | 0/1 | fail | tmpfs/dnotify/lease proc support missing |
| `fcntl33_64` | 0/1 | fail | tmpfs/dnotify/lease proc support missing |
| `fcntl34` | 1/1 | pass |  |
| `fcntl34_64` | 1/1 | pass |  |
| `fcntl35` | 0/1 | fail | /proc/sys/fs/pipe-max-size missing |
| `fcntl35_64` | 0/1 | fail | /proc/sys/fs/pipe-max-size missing |
| `fcntl36_64` | 7/7 | pass | single-case timeout triage: OFD/POSIX lock combinations all synchronized; broad-batch hang not reproduced |
| `fcntl36` | 7/7 | pass | single-case timeout triage: OFD/POSIX lock combinations all synchronized; broad-batch hang not reproduced |
| `fcntl37` | 0/1 | fail | /proc/sys/fs/pipe-max-size missing |
| `fcntl37_64` | 0/1 | fail | /proc/sys/fs/pipe-max-size missing |
| `fcntl38` | 0/1 | fail | CONFIG_DNOTIFY missing |
| `fcntl38_64` | 0/1 | fail | CONFIG_DNOTIFY missing |
| `fcntl39` | 0/1 | fail | CONFIG_DNOTIFY missing |
| `fcntl39_64` | 0/1 | fail | CONFIG_DNOTIFY missing |
| `fdatasync01` | 1/1 | pass | legacy no-summary case; return code 0 |
| `fdatasync02` | 0/2 | fail | legacy no-summary case; return code 1 |
| `fdatasync03` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `fsync01` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `fsync02` | 1/1 | pass |  |
| `fsync03` | 2/5 | partial | fsync errno/return semantics mismatch |
| `fsync04` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `ioctl01` | 0/1 | fail | pty missing |
| `ioctl02` | 0/1 | fail | tty device option missing |
| `ioctl03` | 0/1 | fail | TUN support missing |
| `ioctl04` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `ioctl05` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `ioctl06` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `ioctl07` | 0/1 | fail | /dev/urandom missing |
| `ioctl08` | 0/3 | fail | btrfs/modules unavailable |
| `ioctl09` | 0/1 | fail | helper command/environment unavailable |
| `ioctl_loop01` | 0/3 | fail | loop driver unavailable |
| `ioctl_loop02` | 0/3 | fail | loop driver unavailable |
| `ioctl_loop03` | 0/3 | fail | loop driver unavailable |
| `ioctl_loop04` | 0/3 | fail | loop driver unavailable |
| `ioctl_loop05` | 0/3 | fail | loop driver unavailable |
| `ioctl_loop06` | 0/3 | fail | loop driver unavailable |
| `ioctl_loop07` | 0/3 | fail | loop driver unavailable |
| `ioctl_ns01` | 0/2 | fail | namespace/proc ns entries unavailable |
| `ioctl_ns02` | 0/2 | fail | namespace/proc ns entries unavailable |
| `ioctl_ns03` | 0/2 | fail | namespace/proc ns entries unavailable |
| `ioctl_ns04` | 0/2 | fail | namespace/proc ns entries unavailable |
| `ioctl_ns05` | 0/2 | fail | namespace clone support unavailable |
| `ioctl_ns06` | 0/2 | fail | namespace clone support unavailable |
| `ioctl_ns07` | 4/4 | pass |  |
| `ioctl_sg01` | 0/1 | fail | usable SCSI device unavailable |
| `llseek01` | 1/2 | partial | file-size limit write semantics mismatch |
| `llseek02` | 2/2 | pass |  |
| `llseek03` | 18/18 | pass |  |
| `lseek01` | 4/4 | pass |  |
| `lseek02` | 9/15 | partial | lseek on unsupported fd kinds returned success |
| `lseek07` | 2/2 | pass |  |
| `lseek11` | 0/1 | fail | pwrite ENOSYS |
| `pipe01` | 1/1 | pass |  |
| `pipe02` | 0/2 | fail | single-case timeout triage: `tst_checkpoint_wait` and `tst_checkpoint_wake` both hit ETIMEDOUT; LTP kills case and exits cleanly |
| `pipe03` | 2/2 | pass | fixed wrong-end read/write errno to EBADF |
| `pipe04` | 1/1 | pass | legacy no-summary case; return code 0 |
| `pipe05` | 1/1 | pass | legacy no-summary case; return code 0 |
| `pipe06` | 1/1 | pass |  |
| `pipe07` | 1/2 | partial | fd capacity/accounting mismatch |
| `pipe08` | 1/1 | pass |  |
| `pipe09` | 1/1 | pass | legacy no-summary case; return code 0 |
| `pipe10` | 1/1 | pass |  |
| `pipe11` | 70/70 | pass |  |
| `pipe12` | 1/2 | partial | pipe behavior mismatch |
| `pipe13` | 0/1 | fail | pipe behavior mismatch |
| `pipe14` | 1/1 | pass |  |
| `pipe15` | 0/1 | fail | pipe behavior mismatch |
| `pipe2_01` | 7/7 | pass |  |
| `pipe2_02` | 0/1 | fail | child/helper copy failed |
| `pipe2_04` | 0/1 | fail | F_SETPIPE_SZ returned EBUSY |
| `posix_fadvise01` | 6/6 | pass | /bin/cat shim fixed setup |
| `posix_fadvise01_64` | 6/6 | pass | /bin/cat shim fixed setup |
| `posix_fadvise02` | 6/6 | pass | fadvise64 validation/no-op |
| `posix_fadvise02_64` | 6/6 | pass | fadvise64 validation/no-op |
| `posix_fadvise03` | 32/32 | pass | /bin/cat shim fixed setup |
| `posix_fadvise03_64` | 32/32 | pass | /bin/cat shim fixed setup |
| `posix_fadvise04` | 6/6 | pass | fadvise64 returns ESPIPE for pipes |
| `posix_fadvise04_64` | 6/6 | pass | fadvise64 returns ESPIPE for pipes |
| `pread01` | 1/1 | pass |  |
| `pread01_64` | 1/1 | pass |  |
| `pread02` | 3/3 | pass | fixed positioned I/O ESPIPE on pipe/FIFO |
| `pread02_64` | 3/3 | pass | fixed positioned I/O ESPIPE on pipe/FIFO |
| `preadv01` | 3/3 | pass | preadv implemented through pread64 loop |
| `preadv01_64` | 3/3 | pass | preadv implemented through pread64 loop |
| `preadv02` | 8/8 | pass | preadv errno validation fixed |
| `preadv02_64` | 8/8 | pass | preadv errno validation fixed |
| `preadv03` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `preadv03_64` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `preadv201` | 6/6 | pass | preadv2 implemented via preadv/readv |
| `preadv201_64` | 6/6 | pass | preadv2 implemented via preadv/readv |
| `preadv202` | 8/8 | pass | preadv2 errno validation fixed |
| `preadv202_64` | 8/8 | pass | preadv2 errno validation fixed |
| `preadv203` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `preadv203_64` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `pwrite01` | 1/1 | pass |  |
| `pwrite01_64` | 1/1 | pass |  |
| `pwrite02` | 5/5 | pass | pwrite64 errno validation fixed |
| `pwrite02_64` | 5/5 | pass | pwrite64 errno validation fixed |
| `pwrite03` | 1/1 | pass |  |
| `pwrite03_64` | 1/1 | pass |  |
| `pwrite04` | 1/1 | pass | page-backed O_APPEND handling fixed |
| `pwrite04_64` | 1/1 | pass | page-backed O_APPEND handling fixed |
| `pwritev01` | 3/3 | pass | pwritev implemented through pwrite64 loop |
| `pwritev01_64` | 3/3 | pass | pwritev implemented through pwrite64 loop |
| `pwritev02` | 7/7 | pass | pwritev errno validation fixed |
| `pwritev02_64` | 7/7 | pass | pwritev errno validation fixed |
| `pwritev03` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `pwritev03_64` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `pwritev201` | 6/6 | pass | pwritev2 implemented via pwritev/writev |
| `pwritev201_64` | 6/6 | pass | pwritev2 implemented via pwritev/writev |
| `pwritev202` | 7/7 | pass | pwritev2 errno validation fixed |
| `pwritev202_64` | 7/7 | pass | pwritev2 errno validation fixed |
| `read01` | 1/1 | pass |  |
| `read02` | 3/5 | partial | read errno/edge-case mismatch |
| `read03` | 0/1 | fail | read behavior mismatch |
| `read04` | 1/1 | pass |  |
| `readahead01` | 15/25 | partial | validation-only readahead; remaining fd kinds skipped due missing fd-producing syscalls |
| `readahead02` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `readv01` | 10/10 | pass |  |
| `readv02` | 4/5 | partial | readv edge-case mismatch |
| `sendfile02` | 2/2 | pass |  |
| `sendfile02_64` | 2/2 | pass |  |
| `sendfile03` | 4/4 | pass | fd access errno validation fixed |
| `sendfile03_64` | 4/4 | pass | fd access errno validation fixed |
| `sendfile04` | 5/5 | pass | offset pointer writeability validation fixed |
| `sendfile04_64` | 5/5 | pass | offset pointer writeability validation fixed |
| `sendfile05` | 1/1 | pass | negative offset validation fixed |
| `sendfile05_64` | 1/1 | pass | negative offset validation fixed |
| `sendfile06` | 1/1 | pass |  |
| `sendfile06_64` | 1/1 | pass |  |
| `sendfile07` | - | not-run | not run in this recorded segment |
| `sendfile07_64` | 0/1 | fail | sendfile nonblocking socket EAGAIN still mismatched |
| `sendfile08` | 1/1 | pass |  |
| `sendfile08_64` | 1/1 | pass |  |
| `sendfile09` | 0/1 | fail | large-file sendfile unsupported/mismatch |
| `sendfile09_64` | 0/1 | fail | large-file sendfile unsupported/mismatch |
| `sockioctl01` | 0/8 | fail | legacy no-summary socket ioctl case; return code 3 |
| `splice01` | 0/1 | fail | splice unsupported/mismatch |
| `splice02` | 0/2 | fail | splice unsupported/mismatch |
| `splice03` | 0/7 | fail | splice unsupported/mismatch |
| `splice04` | 0/1 | fail | splice unsupported/mismatch |
| `splice05` | 0/1 | fail | splice unsupported/mismatch |
| `splice06` | 0/1 | fail | splice unsupported/mismatch |
| `splice07` | 217/377 | partial | conservative splice stub passes existing-fd invalid-combination matrix; remaining points need extra fd-producing syscall/fd-kind stubs |
| `splice08` | 0/1 | fail | splice unsupported/mismatch |
| `splice09` | 0/1 | fail | splice unsupported/mismatch |
| `sync01` | 0/2 | fail | sync semantics unsupported/mismatch |
| `sync_file_range01` | 5/5 | pass | validation-only sync_file_range errno handling |
| `sync_file_range02` | 0/2 | fail | needs_device: test_dev.img creation failed with EINVAL |
| `syncfs01` | 0/2 | fail | syncfs unsupported/mismatch |
| `tee01` | 0/1 | fail | tee unsupported/mismatch |
| `tee02` | 0/3 | fail | tee unsupported/mismatch |
| `vmsplice01` | 0/1 | fail | vmsplice unsupported/mismatch |
| `vmsplice02` | 0/3 | fail | vmsplice unsupported/mismatch |
| `vmsplice03` | 0/1 | fail | vmsplice unsupported/mismatch |
| `vmsplice04` | 0/1 | fail | vmsplice unsupported/mismatch |
| `write01` | 1/1 | pass |  |
| `write02` | 2/2 | pass |  |
| `write03` | 1/1 | pass |  |
| `write04` | 0/1 | fail | write behavior mismatch |
| `write05` | 3/3 | pass |  |
| `write06` | 2/2 | pass | page-backed O_APPEND handling fixed |
| `writev01` | 6/6 | pass |  |
| `writev02` | 1/1 | pass | legacy no-summary case; return code 0 |
| `writev03` | 0/1 | fail | writev behavior mismatch |
| `writev05` | 1/1 | pass | legacy no-summary case; return code 0 |
| `writev06` | 1/1 | pass | legacy no-summary case; return code 0 |
| `writev07` | 8/8 | pass |  |

## Failure Notes

- The table is recorded through the end of the current fd-io case list; `sendfile07` hung in an earlier run, while `sendfile07_64` now completes but fails.
- The previous `ioctl05` run exposed `RetiredNodePoolExhausted`; code was adjusted separately in the EBR retired-node pool path. The latest `ioctl05` result is now a normal device-backed test failure, not a kernel panic.
- High-yield fixes already landed in this pass: `pwrite64`, `preadv/pwritev`, `posix_fadvise`, and wrong-end pipe errno. Remaining easy-ish fd-io candidates are `read02/readv02` errno edges, `pwrite04` O_APPEND/file-size semantics, and small sendfile errno cases.
- Device/environment failures are mostly `test_dev.img` acquisition, missing pty/tty/TUN/loop/SCSI/namespace support, and missing helper files/dev nodes.
- Advanced fcntl record-locking cases with `0/0` need source-level inspection; do not count return code alone as pass.

## Next

fd-io has been swept once with known hang cases skipped or worked around. Next useful work is targeted fixes, not rerunning already passing cases.
