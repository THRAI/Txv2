# LTP mount Progress

`mount` batch local tracking. Cases are from `tools/ltp-batches.py --batch mount`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 53 | from `make ltp-batch-cases LTP_BATCH=mount` |
| latest local run | full-image mount batch | 2026-05-30 direct QEMU run scored `9/105` |
| cumulative scored | `9/105` | fresh full-image batch score |
| reached case | `vhangup02` | batch completed |
| logs | `target/oscomp/os_serial_out_ltp_mount_partial_20260530_191652.txt` | latest full-batch serial snapshot |

## 2026-05-30 full-image run

Direct non-Docker QEMU coverage completed all 53 cases in `ltp-batch:mount`,
scoring `9/105`. The guest exited cleanly and fault decode found no kernel trap
lines. The serial snapshot is
`target/oscomp/os_serial_out_ltp_mount_partial_20260530_191652.txt`.

The only fully passing case in this run is `acct01`, which now covers several
error paths and read-only filesystem handling. Most mount-family cases still
break before syscall semantics because LTP cannot create `test_dev.img`
(`EINVAL`) and then cannot acquire a test device. Module tests are blocked by
missing `/proc/cmdline`. `chroot*`, `reboot*`, `pivot_root01`, `unshare*`, and
`setns*` still expose unsupported syscall or namespace surface in the submitted
kernel/image. `setns01/02` are TCONF/TWARN because the guest-side LTP wrapper
reports `__NR_setns` unsupported on this arch path.

Verified with:
`timeout 180s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/testdata --submit target/oscomp/submit --suite ltp-batch:mount`;
`python3 tools/oscomp-judge.py target/oscomp/os_serial_out_ltp_mount_partial_20260530_191652.txt target/oscomp/testdata`;
`cargo xtask fault-decode --target rv64-qemu --serial target/oscomp/os_serial_out_ltp_mount_partial_20260530_191652.txt --all --brief`.

## 2026-05-26 failure notes

- TBROK: 39 recorded case(s); see per-case notes below.
- TFAIL: 9 recorded case(s); see per-case notes below.
- TCONF: 5 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `acct01` | 9/9 | pass | 2026-05-30 full-image run passes normal/error/read-only filesystem paths |
| `acct02` | 0/1 | skip | TCONF: Aborting due to unsuitable kernel config, see above! |
| `chroot01` | 0/1 | fail | TFAIL: unprivileged chroot() expected EPERM: ENOSYS (38) |
| `chroot02` | 0/1 | fail | TFAIL: chroot(/tmp/LTP_chreOLKFA) failed: ENOSYS (38) |
| `chroot03` | 0/5 | fail | TFAIL: chroot(longer than VFS_MAXNAMELEN) expected ENAMETOOLONG: ENOSYS (38) |
| `chroot04` | 0/1 | fail | TFAIL: no search permission chroot() expected EACCES: ENOSYS (38) |
| `delete_module01` | 0/1 | fail | TBROK: fopen(/proc/cmdline,r) failed: ENOENT (2) |
| `delete_module02` | 0/1 | skip | TCONF: syscall(106) __NR_delete_module not supported on your arch |
| `delete_module03` | 0/1 | fail | TBROK: fopen(/proc/cmdline,r) failed: ENOENT (2) |
| `finit_module01` | 0/1 | fail | TBROK: fopen(/proc/cmdline,r) failed: ENOENT (2) |
| `finit_module02` | 0/1 | fail | TBROK: fopen(/proc/cmdline,r) failed: ENOENT (2) |
| `fsconfig01` | 0/2 | fail | TBROK: Failed to acquire device |
| `fsconfig02` | 0/2 | fail | TBROK: Failed to acquire device |
| `fsconfig03` | 0/2 | fail | TBROK: Failed to acquire device |
| `fsmount01` | 0/2 | fail | TBROK: Failed to acquire device |
| `fsmount02` | 0/2 | fail | TBROK: Failed to acquire device |
| `fsopen01` | 0/2 | fail | TBROK: Failed to acquire device |
| `fsopen02` | 0/2 | fail | TBROK: Failed to acquire device |
| `fspick01` | 0/2 | fail | TBROK: Failed to acquire device |
| `fspick02` | 0/2 | fail | TBROK: Failed to acquire device |
| `init_module01` | 0/1 | fail | TBROK: fopen(/proc/cmdline,r) failed: ENOENT (2) |
| `init_module02` | 0/1 | fail | TBROK: fopen(/proc/cmdline,r) failed: ENOENT (2) |
| `mount01` | 0/2 | fail | TBROK: Failed to acquire device |
| `mount02` | 0/2 | fail | TBROK: Failed to acquire device |
| `mount03` | 0/2 | fail | TBROK: Failed to acquire device |
| `mount04` | 0/2 | fail | TBROK: Failed to acquire device |
| `mount05` | 0/2 | fail | TBROK: Failed to acquire device |
| `mount06` | 0/2 | fail | TBROK: Failed to acquire device |
| `mount07` | 0/2 | fail | TBROK: Failed to acquire device |
| `mount_setattr01` | 0/2 | fail | TBROK: Failed to acquire device |
| `move_mount01` | 0/2 | fail | TBROK: Failed to acquire device |
| `move_mount02` | 0/2 | fail | TBROK: Failed to acquire device |
| `open_tree01` | 0/2 | fail | TBROK: Failed to acquire device |
| `open_tree02` | 0/2 | fail | TBROK: Failed to acquire device |
| `pivot_root01` | 0/2 | fail | 2026-05-30 full-image run: `unshare` returns `ENOSYS`; child exits broken |
| `reboot01` | 0/2 | fail | TFAIL: reboot(LINUX_REBOOT_CMD_CAD_ON) failed: ENOSYS (38) |
| `reboot02` | 0/2 | fail | TFAIL: INVALID_CMD expected EINVAL: ENOSYS (38) |
| `setns01` | 0/1 | skip | 2026-05-30 full-image run: LTP wrapper reports `__NR_setns` unsupported |
| `setns02` | 0/3 | skip | 2026-05-30 full-image run: LTP wrapper reports `__NR_setns` unsupported; cleanup warns |
| `swapoff01` | 0/2 | fail | TBROK: Failed to acquire device |
| `swapoff02` | 0/2 | fail | TBROK: Failed to acquire device |
| `swapon01` | 0/2 | fail | TBROK: Failed to acquire device |
| `swapon02` | 0/2 | fail | TBROK: Failed to acquire device |
| `swapon03` | 0/2 | fail | TBROK: Failed to acquire device |
| `umount01` | 0/2 | fail | TBROK: Failed to acquire device |
| `umount02` | 0/2 | fail | TBROK: Failed to acquire device |
| `umount03` | 0/2 | fail | TBROK: Failed to acquire device |
| `umount2_01` | 0/3 | fail | TBROK: tst_device.c:354: Failed to acquire device |
| `umount2_02` | 0/2 | fail | TBROK: Failed to acquire device |
| `unshare01` | 0/3 | fail | 2026-05-30 full-image run: `CLONE_FILES`, `CLONE_FS`, and `CLONE_NEWNS` return `ENOSYS` |
| `unshare02` | 0/2 | fail | 2026-05-30 full-image run: invalid flags and `CLONE_NEWNS` return `ENOSYS` |
| `vhangup01` | 0/1 | skip | TCONF: syscall(58) __NR_vhangup not supported on your arch |
| `vhangup02` | 0/1 | skip | TCONF: syscall(58) __NR_vhangup not supported on your arch |
