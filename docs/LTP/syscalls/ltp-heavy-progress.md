# LTP heavy Progress

`heavy` batch local tracking. Cases are from `tools/ltp-batches.py --batch heavy`.
Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.

## Summary

| Item | Value | Note |
| --- | ---: | --- |
| cases | 67 | from `make ltp-batch-cases LTP_BATCH=heavy` |
| latest local run | `[ltp-musl] 0/2` | 2026-05-26 latest 5-case group |
| cumulative scored | `18/221` | recorded rows in this document |
| reached case | `ustat02` | batch completed |
| logs | `target/oscomp/ltp-progress/heavy` | per-group stdout and serial snapshots |

## 2026-05-26 failure notes

- TCONF: 43 recorded case(s); see per-case notes below.
- TFAIL: 9 recorded case(s); see per-case notes below.
- TBROK: 6 recorded case(s); see per-case notes below.
- EINVAL observed: 1 recorded case(s); see per-case notes below.

## Cases

| Case | Score | Status | Note |
| --- | ---: | --- | --- |
| `arch_prctl01` | 0/1 | skip | TCONF: This arch 'unknown' is not supported for test! |
| `bpf_map01` | 0/1 | skip | TCONF: syscall(280) __NR_bpf not supported on your arch |
| `bpf_prog01` | 0/1 | skip | TCONF: syscall(280) __NR_bpf not supported on your arch |
| `bpf_prog02` | 0/1 | skip | TCONF: syscall(280) __NR_bpf not supported on your arch |
| `bpf_prog03` | 0/1 | skip | TCONF: syscall(280) __NR_bpf not supported on your arch |
| `bpf_prog04` | 0/1 | skip | TCONF: syscall(280) __NR_bpf not supported on your arch |
| `bpf_prog05` | 0/1 | skip | TCONF: syscall(280) __NR_bpf not supported on your arch |
| `bpf_prog06` | 0/1 | skip | TCONF: syscall(280) __NR_bpf not supported on your arch |
| `bpf_prog07` | 0/1 | skip | TCONF: syscall(280) __NR_bpf not supported on your arch |
| `cacheflush01` | 0/1 | skip | TCONF: system doesn't support cacheflush() |
| `getdomainname01` | 1/1 | pass |  |
| `ioperm01` | 0/1 | skip | TCONF: LSB v1.3 does not specify ioperm() for this architecture. (only for i386 or x86_64) |
| `ioperm02` | 0/1 | skip | TCONF: LSB v1.3 does not specify ioperm() for this architecture. (only for i386 or x86_64) |
| `iopl01` | 0/1 | skip | TCONF: LSB v1.3 does not specify iopl() for this architecture. (only for i386 or x86_64) |
| `iopl02` | 0/1 | skip | TCONF: LSB v1.3 does not specify iopl() for this architecture. (only for i386 or x86_64) |
| `modify_ldt01` | 1/1 | pass |  |
| `modify_ldt02` | 1/1 | pass |  |
| `modify_ldt03` | 1/1 | pass |  |
| `newuname01` | 1/1 | pass |  |
| `perf_event_open01` | 0/2 | skip | TCONF: perf_event_open01.c:106: Kernel doesn't have perf_event support |
| `perf_event_open02` | 0/1 | skip | TCONF: Kernel doesn't have perf_event support |
| `perf_event_open03` | 0/1 | skip | TCONF: intel_pt is not available |
| `ptrace01` | 0/3 | fail | TBROK: waitpid(16,0x40202b48,0) failed: ECHILD (10) |
| `ptrace02` | 0/1 | fail | TFAIL: ptrace() expected EPERM, but got: ENOSYS (38) |
| `ptrace03` | 0/1 | fail | TBROK: Failed to open FILE '/proc/sys/kernel/pid_max' for reading: ENOENT (2) |
| `ptrace04` | 0/2 | skip | TCONF: ptrace04.c:103: test not supported for your arch (yet) |
| `ptrace05` | 1/124 | partial | TFAIL: ptrace05.c:96: Failed to ptrace(PTRACE_TRACEME, ...) properly: errno=ENOSYS(38): Function not implemented |
| `ptrace06` | 0/3 | fail | TBROK: spawn_ptrace_child.h:83: child status not stopped: 0x100 |
| `ptrace07` | 0/1 | skip | TCONF: Tests an x86_64 feature |
| `ptrace08` | 0/1 | skip | TCONF: This arch 'unknown' is not supported for test! |
| `ptrace09` | 0/1 | skip | TCONF: This arch 'unknown' is not supported for test! |
| `ptrace10` | 0/1 | skip | TCONF: This arch 'unknown' is not supported for test! |
| `ptrace11` | 0/2 | fail | TBROK: waitpid(1,0,0) failed: ECHILD (10) |
| `quotactl01` | 0/1 | skip | TCONF: Couldn't find 'quotacheck' in $PATH |
| `quotactl02` | 0/1 | skip | TCONF: System doesn't have <xfs/xqm.h> |
| `quotactl03` | 0/1 | skip | TCONF: System doesn't have <xfs/xqm.h> |
| `quotactl04` | 0/1 | skip | TCONF: Couldn't find 'mkfs.ext4' in $PATH |
| `quotactl05` | 0/1 | skip | TCONF: This system didn't have <xfs/xqm.h> |
| `quotactl06` | 0/1 | skip | TCONF: Couldn't find 'quotacheck' in $PATH |
| `quotactl07` | 0/1 | skip | TCONF: System doesn't have <xfs/xqm.h> |
| `quotactl08` | 0/1 | skip | TCONF: Couldn't find 'mkfs.ext4' in $PATH |
| `quotactl09` | 0/1 | skip | TCONF: Couldn't find 'mkfs.ext4' in $PATH |
| `set_thread_area01` | 0/2 | skip | TCONF: set_thread_area01.c:108: set_thread_area isn't available for this architecture |
| `setdomainname01` | 0/2 | fail | TFAIL: setdomainname() failed: 38: ENOSYS (38) |
| `setdomainname02` | 0/6 | fail | TFAIL: unexpected errno: 38, expected: 22: ENOSYS (38) |
| `setdomainname03` | 0/4 | fail | TFAIL: unexpected errno: 38, expected: EPERM: ENOSYS (38) |
| `sethostname01` | 2/2 | pass |  |
| `sethostname02` | 6/6 | pass | EINVAL observed |
| `sethostname03` | 0/2 | fail | TFAIL: unexpected exit code: 0 |
| `sysctl01` | 0/1 | skip | TCONF: syscall(-1) __NR__sysctl not supported on your arch |
| `sysctl03` | 0/1 | skip | TCONF: syscall(-1) __NR__sysctl not supported on your arch |
| `sysctl04` | 0/1 | skip | TCONF: syscall(-1) __NR__sysctl not supported on your arch |
| `sysfs01` | 0/1 | skip | TCONF: syscall(-1) __NR_sysfs not supported on your arch |
| `sysfs02` | 0/1 | skip | TCONF: syscall(-1) __NR_sysfs not supported on your arch |
| `sysfs03` | 0/1 | skip | TCONF: syscall(-1) __NR_sysfs not supported on your arch |
| `sysfs04` | 0/1 | skip | TCONF: syscall(-1) __NR_sysfs not supported on your arch |
| `sysfs05` | 0/1 | skip | TCONF: syscall(-1) __NR_sysfs not supported on your arch |
| `sysinfo01` | 0/1 | fail | TFAIL: sysinfo01.c:105: sysinfo() Failed, errno=38 : Function not implemented |
| `sysinfo02` | 0/1 | fail | TFAIL: sysinfo02.c:107: sysinfo() Failed, Expected -1 returned 38/n |
| `sysinfo03` | 0/1 | skip | TCONF: unshare(128) unsupported: EINVAL (22) |
| `syslog11` | 0/1 | fail | TBROK: Path not found: /proc/sys/kernel/printk: ENOENT (2) |
| `syslog12` | 0/6 | fail | TFAIL: syslog() with invalid type/command succeeded |
| `uname01` | 2/2 | pass |  |
| `uname02` | 1/1 | pass |  |
| `uname04` | 1/2 | partial | TBROK: persona(131072) failed: ENOSYS (38) |
| `ustat01` | 0/1 | skip | TCONF: syscall(-1) __NR_ustat not supported on your arch |
| `ustat02` | 0/1 | skip | TCONF: syscall(-1) __NR_ustat not supported on your arch |
