# LTP Non-Syscalls Runtest Plan

This file tracks LTP runtest modules outside `runtest/syscalls`.
The `p0`/`fd-io`/`vfs` batches split only the `syscalls` runtest file.
The modules below are LTP's native runtest files.

## Commands

List modules:

```bash
make ltp-runtests
```

List entries in one module:

```bash
make ltp-runtest-cases LTP_RUNTEST=fs
```

Run one module on RV64:

```bash
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=fs
```

The guest receives only `ltp-runtest:<module>`, then reads
`/musl/musl/ltp/runtest/<module>` and executes each original runtest command.
This preserves arguments such as `symlink01 -T open01`.

## Suggested Order

| Order | Module | Entries | Why |
| ---: | --- | ---: | --- |
| 1 | `smoketest` | 15 | Fast sanity checks |
| 2 | `fs` | 68 | VFS/file workload after syscall-vfs |
| 3 | `fs_perms_simple` | 18 | Permission behavior |
| 4 | `mm` | 77 | VM after syscall-vm |
| 5 | `syscalls-ipc` | 57 | IPC syscall extension outside syscalls |
| 6 | `ipc` | 6 | Small IPC stress |
| 7 | `pty` | 9 | TTY/PTY support |
| 8 | `sched` | 13 | Scheduler/pthread timing |
| 9 | `commands` | 37 | User command wrappers |
| 10 | `dio` | 30 | Direct I/O if file basics are stable |
| 11 | `cve` | 91 | Mixed regression tests |
| 12 | `fs_readonly` | 55 | Read-only FS/mount behavior |
| 13 | `fs_bind` | 95 | Bind mount behavior |

Defer network (`net*`, `can`), cgroup/container (`controllers`, `containers`),
hardware/security-module heavy tests (`kvm`, `scsi_debug.part1`, `tpm_tools`,
`ima`, `smack`), and large AIO/hugetlb/NUMA stress until core syscall and FS/VM
coverage is stronger.

## Full Module Commands

```bash
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=smoketest
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=fs
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=fs_perms_simple
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=mm
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=syscalls-ipc
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=ipc
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=pty
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=sched
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=commands
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=dio
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=cve
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=fs_readonly
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=fs_bind
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=capability
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=crypto
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=kernel_misc
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=math
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=nptl
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=input
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=watchqueue
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=tracing
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=uevent
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=fcntl-locktests
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=dma_thread_diotest
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=ltp-aio-stress
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=ltp-aiodio.part1
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=ltp-aiodio.part2
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=ltp-aiodio.part3
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=ltp-aiodio.part4
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=hugetlb
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=numa
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=controllers
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=containers
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=cpuhotplug
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=hyperthreading
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=power_management_tests
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=power_management_tests_exclusive
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=crashme
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=irq
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=kvm
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=scsi_debug.part1
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=tpm_tools
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=ima
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=smack
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=s390x_tests
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=can
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.features
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.ipv6
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.ipv6_lib
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.multicast
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.nfs
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.rpc_tests
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.sctp
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.tcp_cmds
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net.tirpc_tests
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.appl
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.broken_ip
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.interface
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.ipsec_dccp
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.ipsec_icmp
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.ipsec_sctp
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.ipsec_tcp
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.ipsec_udp
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.multicast
timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=net_stress.route
```
