# LTP Runtest smoketest Progress

This tracks the native LTP `runtest/smoketest` module, not the Txv2
`syscalls` batch named `smoke`. Native LTP runtest files are allowed to reuse
the same testcase binaries as `runtest/syscalls`, so several entries here are
overlap checks rather than new unique cases.

Run command:

```bash
timeout 600s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=smoketest
```

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| module entries | 15 | from `make ltp-runtest-cases LTP_RUNTEST=smoketest` |
| latest local run | host timeout | 2026-05-26; blocked in `shell_test01` timeout cleanup |
| judge partial score | `154/218` | `tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata`; missing normal group end, so later observed rows are not included |
| observed serial score | `154/220` | includes `splice02` and `df01_sh` from serial before the hang |
| reached case | `shell_test01` | `ping602` and `macsec02` were not reached |
| log | `target/oscomp/ltp-runtests/smoketest/os_serial_out_rv.txt` | copied serial snapshot |

## Notes

- `smoketest` overlaps `runtest/syscalls`: examples include `access01`,
  `chdir01`, `fork01`, `time01`, `wait02`, `write01`, `symlink01`, and the
  `symlink01 -T ...` alias entries.
- `shell_test01` reached LTP's own timeout, printed `Test timed out, sending
  SIGTERM!`, then cleanup looped on `kill(-177) failed: No such process` and
  repeated `cut: /proc/190/stat: No such file or directory` until the host
  `timeout 600s` killed QEMU.
- The symlink-family failures are the same class already seen in syscall VFS
  work: symlink creation/projection behaves like a non-symbolic link or cannot
  be `lstat`ed as expected.

## Cases

| Entry | Command | Score | Status | Note |
| --- | --- | ---: | --- | --- |
| `access01` | `access01` | 147/199 | partial | Non-root DAC checks are too permissive; many `as nobody succeeded` failures |
| `chdir01` | `chdir01` | 0/2 | broken | `tst_device` failed to create/acquire `test_dev.img`: EINVAL / Failed to acquire device |
| `fork01` | `fork01` | 2/2 | pass | child exit status and pid checks passed |
| `time01` | `time01` | 2/2 | pass | `time()` return and stored value matched |
| `wait02` | `wait02` | 1/1 | pass | `wait()` succeeded |
| `write01` | `write01` | 1/1 | pass | `write()` passed |
| `symlink01` | `symlink01` | 1/5 | partial | broken symlink/lstat behavior; only max-path error case passed |
| `stat04` | `symlink01 -T stat04` | 0/3 | broken | symlink target/projection not observed as symbolic link |
| `utime07` | `utime07` | 0/1 | broken | `symlink generated a non-symbolic link` |
| `rename01A` | `symlink01 -T rename01` | 0/2 | broken | symlink target/projection failure in rename alias |
| `splice02` | `splice02 -s 20` | 0/1 | fail | `splice failed: EINVAL (22)` |
| `df01_sh` | `df01.sh` | 0/1 | broken | `tst_device` failed to acquire device |
| `shell_test01` | `echo "SUCCESS" \| shell_pipe01.sh` | 0/0 | hang | LTP timeout fired, but cleanup stuck in process-group/procfs polling |
| `ping602` | `ping02.sh -6` | 0/0 | not-run | not reached after `shell_test01` hang |
| `macsec02` | `macsec02.sh` | 0/0 | not-run | not reached after `shell_test01` hang |
