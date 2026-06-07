# LTP 老式(不计分) / 新式(可计分)测试记录（2026-06-07）

**判据**:LTP 新框架(tst_test)测试运行时输出 `Summary: passed N` 块,judge 据此计分;**老框架测试无 Summary 块 → judge 计 0(不计分)**。

判定方法:在 `metadata/ltp.json` 注册(剥 `_64/_16/尾字母` 变体后缀后匹配)= 新式;否则 = 老式。纯静态判定,与白名单/是否通过无关。

## 全局(去重)
- 老式·不计分: **2568**
- 新式·可计分: **1365**

## 各测试集 老式/新式 计数

| 测试集 | 老式(不计分) | 新式(可计分) |
|---|---|---|
| can | 0 | 3 |
| capability | 5 | 0 |
| commands | 37 | 0 |
| containers | 44 | 40 |
| controllers | 338 | 9 |
| cpuhotplug | 6 | 0 |
| crashme | 3 | 0 |
| crypto | 0 | 10 |
| cve | 80 | 11 |
| dio | 30 | 0 |
| dma_thread_diotest | 7 | 0 |
| fcntl-locktests | 1 | 0 |
| fs | 66 | 2 |
| fs_bind | 95 | 0 |
| fs_perms_simple | 18 | 0 |
| fs_readonly | 55 | 0 |
| hugetlb | 3 | 48 |
| hyperthreading | 2 | 0 |
| ima | 9 | 0 |
| input | 6 | 0 |
| ipc | 6 | 0 |
| irq | 0 | 1 |
| kernel_misc | 13 | 5 |
| kvm | 0 | 5 |
| ltp-aio-stress | 54 | 0 |
| ltp-aiodio.part1 | 140 | 0 |
| ltp-aiodio.part2 | 83 | 0 |
| ltp-aiodio.part3 | 21 | 0 |
| ltp-aiodio.part4 | 58 | 1 |
| math | 10 | 0 |
| mm | 53 | 24 |
| net.features | 61 | 1 |
| net.ipv6 | 11 | 0 |
| net.ipv6_lib | 2 | 4 |
| net.multicast | 4 | 0 |
| net.nfs | 113 | 0 |
| net.rpc_tests | 51 | 0 |
| net.sctp | 41 | 0 |
| net.tcp_cmds | 17 | 0 |
| net.tirpc_tests | 41 | 0 |
| net_stress.appl | 10 | 0 |
| net_stress.broken_ip | 11 | 0 |
| net_stress.interface | 25 | 0 |
| net_stress.ipsec_dccp | 104 | 0 |
| net_stress.ipsec_icmp | 86 | 0 |
| net_stress.ipsec_sctp | 104 | 0 |
| net_stress.ipsec_tcp | 104 | 0 |
| net_stress.ipsec_udp | 106 | 0 |
| net_stress.multicast | 24 | 0 |
| net_stress.route | 14 | 0 |
| nptl | 1 | 0 |
| numa | 12 | 8 |
| power_management_tests | 5 | 0 |
| power_management_tests_exclusive | 5 | 0 |
| pty | 3 | 6 |
| s390x_tests | 1 | 0 |
| sched | 9 | 4 |
| scsi_debug.part1 | 125 | 0 |
| smack | 10 | 0 |
| smoketest | 6 | 9 |
| syscalls | 229 | 1182 |
| syscalls-ipc | 1 | 56 |
| tpm_tools | 12 | 0 |
| tracing | 9 | 0 |
| uevent | 0 | 3 |
| watchqueue | 0 | 9 |

---
# 老式(不计分)用例清单 —— 按测试集

## capability（老式 5）
cap_bounds  check_keepcaps01  check_keepcaps02  check_keepcaps03  filecaps

## commands（老式 37）
ar_sh  cp01_sh  cpio01_sh  df01_sh  du01_sh  file01_sh  gdb01_sh  gzip01_sh  insmod01_sh  keyctl01_sh  ld01_sh  ldd01_sh  ln01_sh  logrotate_sh  lsmod01_sh  mkdir01_sh  mkfs01_btrfs_sh  mkfs01_ext2_sh  mkfs01_ext3_sh  mkfs01_ext4_sh  mkfs01_minix_sh  mkfs01_msdos_sh  mkfs01_ntfs_sh  mkfs01_sh  mkfs01_vfat_sh  mkfs01_xfs_sh  mkswap01_sh  mv01_sh  nm01_sh  shell_test01  sysctl01_sh  sysctl02_sh  tar01_sh  unshare01_sh  unzip01_sh  wc01_sh  which01_sh

## containers（老式 44）
mesgq_nstest_clone  mesgq_nstest_none  mesgq_nstest_unshare  mqns_01_clone  mqns_01_unshare  mqns_02_clone  mqns_02_unshare  mqns_03_clone  mqns_03_unshare  mqns_04_clone  mqns_04_unshare  netns_breakns_ip_ipv4_ioctl  netns_breakns_ip_ipv4_netlink  netns_breakns_ip_ipv6_ioctl  netns_breakns_ip_ipv6_netlink  netns_breakns_ns_exec_ipv4_ioctl  netns_breakns_ns_exec_ipv4_netlink  netns_breakns_ns_exec_ipv6_ioctl  netns_breakns_ns_exec_ipv6_netlink  netns_comm_ip_ipv4_ioctl  netns_comm_ip_ipv4_netlink  netns_comm_ip_ipv6_ioctl  netns_comm_ip_ipv6_netlink  netns_comm_ns_exec_ipv4_ioctl  netns_comm_ns_exec_ipv4_netlink  netns_comm_ns_exec_ipv6_ioctl  netns_comm_ns_exec_ipv6_netlink  netns_sysfs  sem_nstest_clone  sem_nstest_none  sem_nstest_unshare  semtest_2ns_clone  semtest_2ns_none  semtest_2ns_unshare  shmem_2nstest_clone  shmem_2nstest_none  shmem_2nstest_unshare  shmnstest_clone  shmnstest_none  shmnstest_unshare  utsname03_clone  utsname03_unshare  utsname04_clone  utsname04_unshare

## controllers（老式 338）
cgroup  cgroup_fj_function_blkio  cgroup_fj_function_cpu  cgroup_fj_function_cpuacct  cgroup_fj_function_cpuset  cgroup_fj_function_debug  cgroup_fj_function_devices  cgroup_fj_function_freezer  cgroup_fj_function_hugetlb  cgroup_fj_function_memory  cgroup_fj_function_net_cls  cgroup_fj_function_net_prio  cgroup_fj_function_perf_event  cgroup_fj_stress_blkio_10_3_each  cgroup_fj_stress_blkio_10_3_none  cgroup_fj_stress_blkio_10_3_one  cgroup_fj_stress_blkio_1_200_each  cgroup_fj_stress_blkio_1_200_none  cgroup_fj_stress_blkio_1_200_one  cgroup_fj_stress_blkio_200_1_each  cgroup_fj_stress_blkio_200_1_none  cgroup_fj_stress_blkio_200_1_one  cgroup_fj_stress_blkio_2_2_each  cgroup_fj_stress_blkio_2_2_none  cgroup_fj_stress_blkio_2_2_one  cgroup_fj_stress_blkio_2_9_each  cgroup_fj_stress_blkio_2_9_none  cgroup_fj_stress_blkio_2_9_one  cgroup_fj_stress_blkio_3_3_each  cgroup_fj_stress_blkio_3_3_none  cgroup_fj_stress_blkio_3_3_one  cgroup_fj_stress_blkio_4_4_each  cgroup_fj_stress_blkio_4_4_none  cgroup_fj_stress_blkio_4_4_one  cgroup_fj_stress_cpu_10_3_each  cgroup_fj_stress_cpu_10_3_none  cgroup_fj_stress_cpu_10_3_one  cgroup_fj_stress_cpu_1_200_each  cgroup_fj_stress_cpu_1_200_none  cgroup_fj_stress_cpu_1_200_one  cgroup_fj_stress_cpu_200_1_each  cgroup_fj_stress_cpu_200_1_none  cgroup_fj_stress_cpu_200_1_one  cgroup_fj_stress_cpu_2_2_each  cgroup_fj_stress_cpu_2_2_none  cgroup_fj_stress_cpu_2_2_one  cgroup_fj_stress_cpu_2_9_each  cgroup_fj_stress_cpu_2_9_none  cgroup_fj_stress_cpu_2_9_one  cgroup_fj_stress_cpu_3_3_each  cgroup_fj_stress_cpu_3_3_none  cgroup_fj_stress_cpu_3_3_one  cgroup_fj_stress_cpu_4_4_each  cgroup_fj_stress_cpu_4_4_none  cgroup_fj_stress_cpu_4_4_one  cgroup_fj_stress_cpuacct_10_3_each  cgroup_fj_stress_cpuacct_10_3_none  cgroup_fj_stress_cpuacct_10_3_one  cgroup_fj_stress_cpuacct_1_200_each  cgroup_fj_stress_cpuacct_1_200_none  cgroup_fj_stress_cpuacct_1_200_one  cgroup_fj_stress_cpuacct_200_1_each  cgroup_fj_stress_cpuacct_200_1_none  cgroup_fj_stress_cpuacct_200_1_one  cgroup_fj_stress_cpuacct_2_2_each  cgroup_fj_stress_cpuacct_2_2_none  cgroup_fj_stress_cpuacct_2_2_one  cgroup_fj_stress_cpuacct_2_9_each  cgroup_fj_stress_cpuacct_2_9_none  cgroup_fj_stress_cpuacct_2_9_one  cgroup_fj_stress_cpuacct_3_3_each  cgroup_fj_stress_cpuacct_3_3_none  cgroup_fj_stress_cpuacct_3_3_one  cgroup_fj_stress_cpuacct_4_4_each  cgroup_fj_stress_cpuacct_4_4_none  cgroup_fj_stress_cpuacct_4_4_one  cgroup_fj_stress_cpuset_10_3_each  cgroup_fj_stress_cpuset_10_3_none  cgroup_fj_stress_cpuset_10_3_one  cgroup_fj_stress_cpuset_1_200_each  cgroup_fj_stress_cpuset_1_200_none  cgroup_fj_stress_cpuset_1_200_one  cgroup_fj_stress_cpuset_200_1_each  cgroup_fj_stress_cpuset_200_1_none  cgroup_fj_stress_cpuset_200_1_one  cgroup_fj_stress_cpuset_2_2_each  cgroup_fj_stress_cpuset_2_2_none  cgroup_fj_stress_cpuset_2_2_one  cgroup_fj_stress_cpuset_2_9_each  cgroup_fj_stress_cpuset_2_9_none  cgroup_fj_stress_cpuset_2_9_one  cgroup_fj_stress_cpuset_3_3_each  cgroup_fj_stress_cpuset_3_3_none  cgroup_fj_stress_cpuset_3_3_one  cgroup_fj_stress_cpuset_4_4_each  cgroup_fj_stress_cpuset_4_4_none  cgroup_fj_stress_cpuset_4_4_one  cgroup_fj_stress_debug_10_3_each  cgroup_fj_stress_debug_10_3_none  cgroup_fj_stress_debug_10_3_one  cgroup_fj_stress_debug_1_200_each  cgroup_fj_stress_debug_1_200_none  cgroup_fj_stress_debug_1_200_one  cgroup_fj_stress_debug_200_1_each  cgroup_fj_stress_debug_200_1_none  cgroup_fj_stress_debug_200_1_one  cgroup_fj_stress_debug_2_2_each  cgroup_fj_stress_debug_2_2_none  cgroup_fj_stress_debug_2_2_one  cgroup_fj_stress_debug_2_9_each  cgroup_fj_stress_debug_2_9_none  cgroup_fj_stress_debug_2_9_one  cgroup_fj_stress_debug_3_3_each  cgroup_fj_stress_debug_3_3_none  cgroup_fj_stress_debug_3_3_one  cgroup_fj_stress_debug_4_4_each  cgroup_fj_stress_debug_4_4_none  cgroup_fj_stress_debug_4_4_one  cgroup_fj_stress_devices_10_3_each  cgroup_fj_stress_devices_10_3_none  cgroup_fj_stress_devices_10_3_one  cgroup_fj_stress_devices_1_200_each  cgroup_fj_stress_devices_1_200_none  cgroup_fj_stress_devices_1_200_one  cgroup_fj_stress_devices_200_1_each  cgroup_fj_stress_devices_200_1_none  cgroup_fj_stress_devices_200_1_one  cgroup_fj_stress_devices_2_2_each  cgroup_fj_stress_devices_2_2_none  cgroup_fj_stress_devices_2_2_one  cgroup_fj_stress_devices_2_9_each  cgroup_fj_stress_devices_2_9_none  cgroup_fj_stress_devices_2_9_one  cgroup_fj_stress_devices_3_3_each  cgroup_fj_stress_devices_3_3_none  cgroup_fj_stress_devices_3_3_one  cgroup_fj_stress_devices_4_4_each  cgroup_fj_stress_devices_4_4_none  cgroup_fj_stress_devices_4_4_one  cgroup_fj_stress_freezer_10_3_each  cgroup_fj_stress_freezer_10_3_none  cgroup_fj_stress_freezer_10_3_one  cgroup_fj_stress_freezer_1_200_each  cgroup_fj_stress_freezer_1_200_none  cgroup_fj_stress_freezer_1_200_one  cgroup_fj_stress_freezer_200_1_each  cgroup_fj_stress_freezer_200_1_none  cgroup_fj_stress_freezer_200_1_one  cgroup_fj_stress_freezer_2_2_each  cgroup_fj_stress_freezer_2_2_none  cgroup_fj_stress_freezer_2_2_one  cgroup_fj_stress_freezer_2_9_each  cgroup_fj_stress_freezer_2_9_none  cgroup_fj_stress_freezer_2_9_one  cgroup_fj_stress_freezer_3_3_each  cgroup_fj_stress_freezer_3_3_none  cgroup_fj_stress_freezer_3_3_one  cgroup_fj_stress_freezer_4_4_each  cgroup_fj_stress_freezer_4_4_none  cgroup_fj_stress_freezer_4_4_one  cgroup_fj_stress_hugetlb_10_3_each  cgroup_fj_stress_hugetlb_10_3_none  cgroup_fj_stress_hugetlb_10_3_one  cgroup_fj_stress_hugetlb_1_200_each  cgroup_fj_stress_hugetlb_1_200_none  cgroup_fj_stress_hugetlb_1_200_one  cgroup_fj_stress_hugetlb_200_1_each  cgroup_fj_stress_hugetlb_200_1_none  cgroup_fj_stress_hugetlb_200_1_one  cgroup_fj_stress_hugetlb_2_2_each  cgroup_fj_stress_hugetlb_2_2_none  cgroup_fj_stress_hugetlb_2_2_one  cgroup_fj_stress_hugetlb_2_9_each  cgroup_fj_stress_hugetlb_2_9_none  cgroup_fj_stress_hugetlb_2_9_one  cgroup_fj_stress_hugetlb_3_3_each  cgroup_fj_stress_hugetlb_3_3_none  cgroup_fj_stress_hugetlb_3_3_one  cgroup_fj_stress_hugetlb_4_4_each  cgroup_fj_stress_hugetlb_4_4_none  cgroup_fj_stress_hugetlb_4_4_one  cgroup_fj_stress_memory_10_3_each  cgroup_fj_stress_memory_10_3_none  cgroup_fj_stress_memory_10_3_one  cgroup_fj_stress_memory_1_200_each  cgroup_fj_stress_memory_1_200_none  cgroup_fj_stress_memory_1_200_one  cgroup_fj_stress_memory_200_1_each  cgroup_fj_stress_memory_200_1_none  cgroup_fj_stress_memory_200_1_one  cgroup_fj_stress_memory_2_2_each  cgroup_fj_stress_memory_2_2_none  cgroup_fj_stress_memory_2_2_one  cgroup_fj_stress_memory_2_9_each  cgroup_fj_stress_memory_2_9_none  cgroup_fj_stress_memory_2_9_one  cgroup_fj_stress_memory_3_3_each  cgroup_fj_stress_memory_3_3_none  cgroup_fj_stress_memory_3_3_one  cgroup_fj_stress_memory_4_4_each  cgroup_fj_stress_memory_4_4_none  cgroup_fj_stress_memory_4_4_one  cgroup_fj_stress_net_cls_10_3_each  cgroup_fj_stress_net_cls_10_3_none  cgroup_fj_stress_net_cls_10_3_one  cgroup_fj_stress_net_cls_1_200_each  cgroup_fj_stress_net_cls_1_200_none  cgroup_fj_stress_net_cls_1_200_one  cgroup_fj_stress_net_cls_200_1_each  cgroup_fj_stress_net_cls_200_1_none  cgroup_fj_stress_net_cls_200_1_one  cgroup_fj_stress_net_cls_2_2_each  cgroup_fj_stress_net_cls_2_2_none  cgroup_fj_stress_net_cls_2_2_one  cgroup_fj_stress_net_cls_2_9_each  cgroup_fj_stress_net_cls_2_9_none  cgroup_fj_stress_net_cls_2_9_one  cgroup_fj_stress_net_cls_3_3_each  cgroup_fj_stress_net_cls_3_3_none  cgroup_fj_stress_net_cls_3_3_one  cgroup_fj_stress_net_cls_4_4_each  cgroup_fj_stress_net_cls_4_4_none  cgroup_fj_stress_net_cls_4_4_one  cgroup_fj_stress_net_prio_10_3_each  cgroup_fj_stress_net_prio_10_3_none  cgroup_fj_stress_net_prio_10_3_one  cgroup_fj_stress_net_prio_1_200_each  cgroup_fj_stress_net_prio_1_200_none  cgroup_fj_stress_net_prio_1_200_one  cgroup_fj_stress_net_prio_200_1_each  cgroup_fj_stress_net_prio_200_1_none  cgroup_fj_stress_net_prio_200_1_one  cgroup_fj_stress_net_prio_2_2_each  cgroup_fj_stress_net_prio_2_2_none  cgroup_fj_stress_net_prio_2_2_one  cgroup_fj_stress_net_prio_2_9_each  cgroup_fj_stress_net_prio_2_9_none  cgroup_fj_stress_net_prio_2_9_one  cgroup_fj_stress_net_prio_3_3_each  cgroup_fj_stress_net_prio_3_3_none  cgroup_fj_stress_net_prio_3_3_one  cgroup_fj_stress_net_prio_4_4_each  cgroup_fj_stress_net_prio_4_4_none  cgroup_fj_stress_net_prio_4_4_one  cgroup_fj_stress_perf_event_10_3_each  cgroup_fj_stress_perf_event_10_3_none  cgroup_fj_stress_perf_event_10_3_one  cgroup_fj_stress_perf_event_1_200_each  cgroup_fj_stress_perf_event_1_200_none  cgroup_fj_stress_perf_event_1_200_one  cgroup_fj_stress_perf_event_200_1_each  cgroup_fj_stress_perf_event_200_1_none  cgroup_fj_stress_perf_event_200_1_one  cgroup_fj_stress_perf_event_2_2_each  cgroup_fj_stress_perf_event_2_2_none  cgroup_fj_stress_perf_event_2_2_one  cgroup_fj_stress_perf_event_2_9_each  cgroup_fj_stress_perf_event_2_9_none  cgroup_fj_stress_perf_event_2_9_one  cgroup_fj_stress_perf_event_3_3_each  cgroup_fj_stress_perf_event_3_3_none  cgroup_fj_stress_perf_event_3_3_one  cgroup_fj_stress_perf_event_4_4_each  cgroup_fj_stress_perf_event_4_4_none  cgroup_fj_stress_perf_event_4_4_one  cgroup_xattr  controllers  cpuacct_100_1  cpuacct_100_100  cpuacct_10_10  cpuacct_1_1  cpuacct_1_10  cpuacct_1_100  cpuset_base_ops  cpuset_exclusive  cpuset_hierarchy  cpuset_hotplug  cpuset_inherit  cpuset_load_balance  cpuset_memory  cpuset_memory_pressure  cpuset_memory_spread  cpuset_regression_test  cpuset_sched_domains  cpuset_syscall  memcg_control  memcg_failcnt  memcg_force_empty  memcg_limit_in_bytes  memcg_max_usage_in_bytes  memcg_memsw_limit_in_bytes  memcg_move_charge_at_immigrate  memcg_regression  memcg_stat  memcg_stat_rss  memcg_stress  memcg_subgroup_charge  memcg_usage_in_bytes  memcg_use_hierarchy  pids_1_1  pids_1_10  pids_1_100  pids_1_2  pids_1_50  pids_2_1  pids_2_10  pids_2_100  pids_2_2  pids_2_50  pids_3_0  pids_3_1  pids_3_10  pids_3_100  pids_3_50  pids_4_1  pids_4_10  pids_4_100  pids_4_2  pids_4_50  pids_5_1  pids_6_1  pids_6_10  pids_6_100  pids_6_2  pids_6_50  pids_7_10  pids_7_100  pids_7_1000  pids_7_50  pids_7_500  pids_8_10  pids_8_100  pids_8_2  pids_8_50  pids_9_10  pids_9_100  pids_9_2  pids_9_50

## cpuhotplug（老式 6）
cpuhotplug02  cpuhotplug03  cpuhotplug04  cpuhotplug05  cpuhotplug06  cpuhotplug07

## crashme（老式 3）
crash01  crash02  f00f

## cve（老式 80）
cve-2011-0999  cve-2011-2183  cve-2011-2496  cve-2012-0957  cve-2015-0235  cve-2015-7550  cve-2016-4470  cve-2016-4997  cve-2016-5195  cve-2016-8655  cve-2016-9604  cve-2016-9793  cve-2017-1000111  cve-2017-1000112  cve-2017-1000364  cve-2017-1000380  cve-2017-1000405  cve-2017-10661  cve-2017-12192  cve-2017-12193  cve-2017-15274  cve-2017-15299  cve-2017-15537  cve-2017-15649  cve-2017-15951  cve-2017-16995  cve-2017-17712  cve-2017-17805  cve-2017-17806  cve-2017-17807  cve-2017-18075  cve-2017-18344  cve-2017-2636  cve-2017-5754  cve-2017-6951  cve-2017-7308  cve-2017-7472  cve-2017-7616  cve-2017-8890  cve-2018-1000001  cve-2018-1000199  cve-2018-1000204  cve-2018-10124  cve-2018-11508  cve-2018-12896  cve-2018-13405  cve-2018-18445  cve-2018-18559  cve-2018-18955  cve-2018-19854  cve-2018-5803  cve-2018-6927  cve-2018-7566  cve-2018-8897  cve-2018-9568  cve-2019-8912  cve-2020-11494  cve-2020-14386  cve-2020-14416  cve-2020-25704  cve-2020-25705  cve-2020-29373  cve-2020-36557  cve-2021-22555  cve-2021-22600  cve-2021-26708  cve-2021-3444  cve-2021-3609  cve-2021-38604  cve-2021-4034  cve-2021-4197_1  cve-2021-4197_2  cve-2021-4204  cve-2022-0185  cve-2022-0847  cve-2022-23222  cve-2022-2590  cve-2023-0461  cve-2023-1829  cve-2023-31248

## dio（老式 30）
dio01  dio02  dio03  dio04  dio05  dio06  dio07  dio08  dio09  dio10  dio11  dio12  dio13  dio14  dio15  dio16  dio17  dio18  dio19  dio20  dio21  dio22  dio23  dio24  dio25  dio26  dio27  dio28  dio29  dio30

## dma_thread_diotest（老式 7）
dma_thread_diotest1  dma_thread_diotest2  dma_thread_diotest3  dma_thread_diotest4  dma_thread_diotest5  dma_thread_diotest6  dma_thread_diotest7

## fcntl-locktests（老式 1）
FCNTL_LOCKTESTS

## fs（老式 66）
binfmt_misc01  binfmt_misc02  fs_di  fs_inod01  fs_racer  ftest01  ftest02  ftest03  ftest04  ftest05  ftest06  ftest07  ftest08  gf01  gf02  gf03  gf04  gf05  gf06  gf07  gf08  gf09  gf10  gf11  gf12  gf13  gf14  gf15  gf16  gf17  gf18  gf19  gf20  gf21  gf22  gf23  gf24  gf25  gf26  gf27  gf28  gf29  gf30  inode01  inode02  iogen01  isofs  lftest01  linker01  openfile01  proc01  quota_remount_test01  read_all_dev  read_all_proc  read_all_sys  rwtest01  rwtest02  rwtest03  rwtest04  rwtest05  stream01  stream02  stream03  stream04  stream05  writetest01

## fs_bind（老式 95）
fs_bind01_sh  fs_bind02_sh  fs_bind03_sh  fs_bind04_sh  fs_bind05_sh  fs_bind06_sh  fs_bind07-2_sh  fs_bind07_sh  fs_bind08_sh  fs_bind09_sh  fs_bind10_sh  fs_bind11_sh  fs_bind12_sh  fs_bind13_sh  fs_bind14_sh  fs_bind15_sh  fs_bind16_sh  fs_bind17_sh  fs_bind18_sh  fs_bind19_sh  fs_bind20_sh  fs_bind21_sh  fs_bind22_sh  fs_bind23_sh  fs_bind24_sh  fs_bind_cloneNS01_sh  fs_bind_cloneNS02_sh  fs_bind_cloneNS03_sh  fs_bind_cloneNS04_sh  fs_bind_cloneNS05_sh  fs_bind_cloneNS06_sh  fs_bind_cloneNS07_sh  fs_bind_move01_sh  fs_bind_move02_sh  fs_bind_move03_sh  fs_bind_move04_sh  fs_bind_move05_sh  fs_bind_move06_sh  fs_bind_move07_sh  fs_bind_move08_sh  fs_bind_move09_sh  fs_bind_move10_sh  fs_bind_move11_sh  fs_bind_move12_sh  fs_bind_move13_sh  fs_bind_move14_sh  fs_bind_move15_sh  fs_bind_move16_sh  fs_bind_move17_sh  fs_bind_move18_sh  fs_bind_move19_sh  fs_bind_move20_sh  fs_bind_move21_sh  fs_bind_move22_sh  fs_bind_rbind01_sh  fs_bind_rbind02_sh  fs_bind_rbind03_sh  fs_bind_rbind04_sh  fs_bind_rbind05_sh  fs_bind_rbind06_sh  fs_bind_rbind07-2_sh  fs_bind_rbind07_sh  fs_bind_rbind08_sh  fs_bind_rbind09_sh  fs_bind_rbind10_sh  fs_bind_rbind11_sh  fs_bind_rbind12_sh  fs_bind_rbind13_sh  fs_bind_rbind14_sh  fs_bind_rbind15_sh  fs_bind_rbind16_sh  fs_bind_rbind17_sh  fs_bind_rbind18_sh  fs_bind_rbind19_sh  fs_bind_rbind20_sh  fs_bind_rbind21_sh  fs_bind_rbind22_sh  fs_bind_rbind23_sh  fs_bind_rbind24_sh  fs_bind_rbind25_sh  fs_bind_rbind26_sh  fs_bind_rbind27_sh  fs_bind_rbind28_sh  fs_bind_rbind29_sh  fs_bind_rbind30_sh  fs_bind_rbind31_sh  fs_bind_rbind32_sh  fs_bind_rbind33_sh  fs_bind_rbind34_sh  fs_bind_rbind35_sh  fs_bind_rbind36_sh  fs_bind_rbind37_sh  fs_bind_rbind38_sh  fs_bind_rbind39_sh  fs_bind_regression_sh

## fs_perms_simple（老式 18）
fs_perms01  fs_perms02  fs_perms03  fs_perms04  fs_perms05  fs_perms06  fs_perms07  fs_perms08  fs_perms09  fs_perms10  fs_perms11  fs_perms12  fs_perms13  fs_perms14  fs_perms15  fs_perms16  fs_perms17  fs_perms18

## fs_readonly（老式 55）
test_robind01  test_robind02  test_robind03  test_robind04  test_robind05  test_robind06  test_robind07  test_robind08  test_robind09  test_robind10  test_robind11  test_robind12  test_robind13  test_robind14  test_robind15  test_robind16  test_robind17  test_robind18  test_robind19  test_robind20  test_robind21  test_robind22  test_robind23  test_robind24  test_robind25  test_robind26  test_robind27  test_robind28  test_robind29  test_robind30  test_robind31  test_robind32  test_robind33  test_robind34  test_robind35  test_robind36  test_robind37  test_robind38  test_robind39  test_robind40  test_robind41  test_robind42  test_robind43  test_robind44  test_robind45  test_robind46  test_robind47  test_robind48  test_robind49  test_robind50  test_robind51  test_robind52  test_robind53  test_robind54  test_robind55

## hugetlb（老式 3）
hugemmap05_1  hugemmap05_2  hugemmap05_3

## hyperthreading（老式 2）
smt_smp_affinity  smt_smp_enabled

## ima（老式 9）
evm_overlay  ima_conditionals  ima_kexec  ima_keys  ima_measurements  ima_policy  ima_selinux  ima_tpm  ima_violations

## input（老式 6）
input01  input02  input03  input04  input05  input06

## ipc（老式 6）
pipeio_1  pipeio_3  pipeio_4  pipeio_5  pipeio_6  pipeio_8

## kernel_misc（老式 13）
block_dev  cn_pec_sh  cpufreq_boost  fw_load  lock_torture  ltp_acpi  rcu_torture  rtc01  tbio  tpci  uaccess  zram01  zram02

## ltp-aio-stress（老式 54）
ADS1000  ADS1001  ADS1002  ADS1003  ADS1004  ADS1005  ADS1006  ADS1007  ADS1008  ADS1009  ADS1010  ADS1011  ADS1012  ADS1013  ADS1014  ADS1015  ADS1016  ADS1017  ADS1018  ADS1019  ADS1020  ADS1021  ADS1022  ADS1023  ADS1024  ADS1025  ADS1026  ADS1027  ADS1028  ADS1029  ADS1030  ADS1031  ADS1032  ADS1033  ADS1034  ADS1035  ADS1036  ADS1037  ADS1038  ADS1039  ADS1040  ADS1041  ADS1042  ADS1043  ADS1044  ADS1045  ADS1046  ADS1047  ADS1048  ADS1049  ADS1050  ADS1051  ADS1052  ADS1053

## ltp-aiodio.part1（老式 140）
AD001  AD002  AD003  AD004  AD005  AD006  AD007  AD008  AD009  AD010  AD011  AD012  AD013  AD014  AD015  AD016  AD017  AD018  AD019  AD020  AD021  AD022  AD023  AD024  AD025  AD026  AD027  AD028  AD029  AD030  AD031  AD032  AD033  AD034  AD035  AD036  AD037  AD038  AD039  AD040  AD041  AD042  AD043  AD044  AD045  AD046  AD047  AD048  AD049  AD050  AD051  AD052  AD053  AD054  AD055  AD056  AD057  AD058  AD059  AD060  AD061  AD062  AD063  AD064  AD065  AD066  AD067  AD068  AD069  AD070  AD071  AD072  AD073  AD074  AD075  AD076  AD077  AD078  AD079  AD080  AD081  AD082  AD083  AD084  AD085  AD086  AD087  AD088  AD089  AD090  AD091  AD092  AD093  AD094  AD095  AD096  AD097  AD098  AD099  AD100  AD101  AD102  AD103  AD104  AD105  AD106  AD107  AD108  AD109  AD110  AD111  AD112  AD113  AD114  AD115  AD116  AD117  AD118  AD119  AD120  AD121  AD122  AD123  AD124  AD125  AD126  AD127  AD128  AD129  AD130  AD131  AD132  AD133  AD134  AD135  AD136  AD137  AD138  AD139  AD140

## ltp-aiodio.part2（老式 83）
ADSP000  ADSP001  ADSP002  ADSP003  ADSP004  ADSP005  ADSP006  ADSP007  ADSP008  ADSP009  ADSP010  ADSP011  ADSP012  ADSP013  ADSP014  ADSP015  ADSP016  ADSP017  ADSP018  ADSP019  ADSP020  ADSP021  ADSP022  ADSP023  ADSP024  ADSP025  ADSP026  ADSP027  ADSP028  ADSP029  ADSP030  ADSP031  ADSP032  ADSP033  ADSP034  ADSP035  ADSP036  ADSP037  ADSP038  ADSP039  ADSP040  ADSP041  ADSP042  ADSP043  ADSP044  ADSP045  ADSP046  ADSP047  ADSP048  ADSP049  ADSP050  ADSP051  ADSP052  ADSP053  ADSP054  ADSP055  ADSP056  ADSP057  ADSP058  ADSP059  ADSP060  ADSP061  ADSP062  ADSP063  ADSP064  ADSP065  ADSP066  ADSP067  ADSP068  ADSP069  ADSP070  ADSP071  ADSP072  ADSP073  ADSP074  ADSP075  ADSP076  ADSP077  ADSP078  ADSP079  ADSP080  ADSP081  ADSP082

## ltp-aiodio.part3（老式 21）
fsx01  fsx02  fsx03  fsx04  fsx05  fsx06  fsx07  fsx08  fsx09  fsx10  fsx12  fsx13  fsx14  fsx15  fsx16  fsx17  fsx18  fsx19  fsx20  fsx21  fsx22

## ltp-aiodio.part4（老式 58）
AD000  AD001  AD002  AD003  AD004  AD005  AD006  AD007  AD008  AD009  ADI000  ADI001  ADI002  ADI003  ADI004  ADI005  ADI006  ADI007  ADI008  ADI009  DI000  DI001  DI002  DI003  DI004  DI005  DI006  DI007  DI008  DI009  DIO00  DIO01  DIO02  DIO03  DIO04  DIO05  DIO06  DIO07  DIO08  DIO09  DIT000  DIT001  DIT002  DOR000  DOR001  DOR002  DOR003  DS000  DS001  DS002  DS003  DS004  DS005  DS006  DS007  DS008  DS009  aio01

## math（老式 10）
abs01  atof01  float_bessel  float_exp_log  float_iperb  float_power  float_trigo  fptest01  fptest02  nextafter01

## mm（老式 53）
data_space  ksm01_1  ksm02_1  ksm03_1  ksm04_1  ksm06_1  ksm06_2  mallocstress01  mem02  mm01  mm02  mmap10  mmap10_1  mmap10_2  mmap10_3  mmap10_4  mmapstress02  mmapstress03  mmapstress05  mmapstress06  mmapstress07  mmapstress08  mmapstress09  mmapstress10  mtest01w  mtest05  mtest06  mtest06_2  mtest06_3  overcommit_memory01  overcommit_memory02  overcommit_memory03  overcommit_memory04  overcommit_memory05  overcommit_memory06  page01  page02  shm_test01  shmt02  shmt03  shmt04  shmt05  shmt06  shmt07  shmt08  shmt09  shmt10  stack_space  vma01  vma02  vma03  vma04  vma05

## net.features（老式 61）
bbr01  bbr01_ipv6  bbr02  bbr02_ipv6  bind_noport01  bind_noport01_ipv6  busy_poll01  busy_poll01_ipv6  busy_poll02  busy_poll02_ipv6  busy_poll03  busy_poll03_ipv6  dccp01  dccp01_ipv6  dctcp_ipv4_01  dctcp_ipv6_01  fou01  fou01_ipv6  geneve01  geneve01_ipv6  geneve02  geneve02_ipv6  gre_ipv4_01  gre_ipv4_02  gre_ipv6_01  gre_ipv6_02  gue01  gue01_ipv6  ipvlan01  macsec01  macsec02  macsec03  macvlan01  macvtap01  mpls01  mpls02  mpls02_ipv6  mpls03  mpls03_ipv6  mpls04  sctp01  sctp01_ipv6  sit01  tcp_fastopen  tcp_fastopen6  vlan01  vlan02  vlan03  vxlan01  vxlan02  vxlan02_ipv6  vxlan_ipv6_multi_03  vxlan_ipv6_uni_03  vxlan_ipv6_uni_04  vxlan_multi_03  vxlan_uni_03  vxlan_uni_04  wireguard01  wireguard01_ipv6  wireguard02  wireguard02_ipv6

## net.ipv6（老式 11）
dhcpd6  dnsmasq6  ip6tables  ipneigh6_ip  nft6  ping601  ping602  sendfile601  tcpdump601  tracepath601  traceroute601

## net.ipv6_lib（老式 2）
asapi_01  asapi_03

## net.multicast（老式 4）
mc_cmds  mc_commo  mc_member  mc_opts

## net.nfs（老式 113）
fsx_v30_ip4t  fsx_v30_ip4u  fsx_v30_ip6t  fsx_v30_ip6u  fsx_v40_ip4t  fsx_v40_ip6t  fsx_v41_ip4t  fsx_v41_ip6t  fsx_v42_ip4t  fsx_v42_ip6t  nfs01_v30_ip4t  nfs01_v30_ip4u  nfs01_v30_ip6t  nfs01_v30_ip6u  nfs01_v40_ip4t  nfs01_v40_ip6t  nfs01_v41_ip4t  nfs01_v41_ip6t  nfs01_v42_ip4t  nfs01_v42_ip6t  nfs02_v30_ip4t  nfs02_v30_ip4u  nfs02_v30_ip6t  nfs02_v30_ip6u  nfs02_v40_ip4t  nfs02_v40_ip6t  nfs02_v41_ip4t  nfs02_v41_ip6t  nfs02_v42_ip4t  nfs02_v42_ip6t  nfs03_v30_ip4t  nfs03_v30_ip4u  nfs03_v30_ip6t  nfs03_v30_ip6u  nfs03_v40_ip4t  nfs03_v40_ip6t  nfs03_v41_ip4t  nfs03_v41_ip6t  nfs03_v42_ip4t  nfs03_v42_ip6t  nfs04_v30_ip4t  nfs04_v30_ip4u  nfs04_v30_ip6t  nfs04_v30_ip6u  nfs04_v40_ip4t  nfs04_v40_ip6t  nfs04_v41_ip4t  nfs04_v41_ip6t  nfs04_v42_ip4t  nfs04_v42_ip6t  nfs05_v30_ip4t  nfs05_v30_ip4u  nfs05_v30_ip6t  nfs05_v30_ip6u  nfs05_v40_ip4t  nfs05_v40_ip6t  nfs05_v41_ip4t  nfs05_v41_ip6t  nfs05_v42_ip4t  nfs05_v42_ip6t  nfs06_v30_v40_ip4  nfs06_v4x_ip6t  nfs06_vall_ip4t  nfs07_v30_ip4t  nfs07_v30_ip4u  nfs07_v30_ip6t  nfs07_v30_ip6u  nfs07_v40_ip4t  nfs07_v40_ip6t  nfs07_v41_ip4t  nfs07_v41_ip6t  nfs07_v42_ip4t  nfs07_v42_ip6t  nfs08_v30_ip4t  nfs08_v30_ip4u  nfs08_v30_ip6t  nfs08_v30_ip6u  nfs08_v40_ip4t  nfs08_v40_ip6t  nfs08_v41_ip4t  nfs08_v41_ip6t  nfs08_v42_ip4t  nfs08_v42_ip6t  nfs09_v30_ip4t  nfs09_v30_ip4u  nfs09_v30_ip6t  nfs09_v30_ip6u  nfs09_v40_ip4t  nfs09_v40_ip6t  nfs09_v41_ip4t  nfs09_v41_ip6t  nfs09_v42_ip4t  nfs09_v42_ip6t  nfslock01_v30_ip4t  nfslock01_v30_ip4u  nfslock01_v30_ip6t  nfslock01_v30_ip6u  nfslock01_v40_ip4t  nfslock01_v40_ip6t  nfslock01_v41_ip4t  nfslock01_v41_ip6t  nfslock01_v42_ip4t  nfslock01_v42_ip6t  nfsstat01_v30_ip4t  nfsstat01_v30_ip4u  nfsstat01_v30_ip6t  nfsstat01_v30_ip6u  nfsstat01_v40_ip4t  nfsstat01_v40_ip6t  nfsstat01_v41_ip4t  nfsstat01_v41_ip6t  nfsstat01_v42_ip4t  nfsstat01_v42_ip6t

## net.rpc_tests（老式 51）
rpc01  rpc_auth_destroy  rpc_authnone_create  rpc_authunix_create  rpc_authunix_create_default  rpc_callrpc  rpc_clnt_broadcast  rpc_clnt_call  rpc_clnt_control  rpc_clnt_create  rpc_clnt_destroy  rpc_clnt_freeres  rpc_clnt_geterr  rpc_clnt_pcreateerror  rpc_clnt_perrno  rpc_clnt_perror  rpc_clnt_spcreateerror  rpc_clnt_sperrno  rpc_clnt_sperror  rpc_clntraw_create  rpc_clnttcp_create  rpc_clntudp_bufcreate  rpc_clntudp_create  rpc_get_myaddress  rpc_pmap_getmaps  rpc_pmap_getport  rpc_pmap_rmtcall  rpc_pmap_set  rpc_pmap_unset  rpc_registerrpc  rpc_svc_destroy  rpc_svc_freeargs  rpc_svc_getargs  rpc_svc_getcaller  rpc_svc_register  rpc_svc_sendreply  rpc_svc_unregister  rpc_svcerr_auth  rpc_svcerr_noproc  rpc_svcerr_noprog  rpc_svcerr_progvers  rpc_svcerr_systemerr  rpc_svcerr_weakauth  rpc_svcfd_create  rpc_svcraw_create  rpc_svctcp_create  rpc_svcudp_bufcreate  rpc_svcudp_create  rpc_xprt_register  rpc_xprt_unregister  rpcinfo

## net.sctp（老式 41）
test_1_to_1_accept_close  test_1_to_1_addrs  test_1_to_1_connect  test_1_to_1_connectx  test_1_to_1_events  test_1_to_1_initmsg_connect  test_1_to_1_nonblock  test_1_to_1_recvfrom  test_1_to_1_recvmsg  test_1_to_1_rtoinfo  test_1_to_1_send  test_1_to_1_sendmsg  test_1_to_1_sendto  test_1_to_1_shutdown  test_1_to_1_socket_bind_listen  test_1_to_1_sockopt  test_1_to_1_threads  test_assoc_abort  test_assoc_shutdown  test_autoclose  test_basic  test_basic_v6  test_connect  test_connectx  test_fragments  test_fragments_v6  test_getname  test_getname_v6  test_inaddr_any  test_inaddr_any_v6  test_peeloff  test_peeloff_v6  test_recvmsg  test_sctp_sendrecvmsg  test_sctp_sendrecvmsg_v6  test_sockopt  test_sockopt_v6  test_tcp_style  test_tcp_style_v6  test_timetolive  test_timetolive_v6

## net.tcp_cmds（老式 17）
arping01  dhcpd  dnsmasq  ftp  ipneigh01_arp  ipneigh01_ip  iproute  iptables  netstat  nft  ping01  ping02  sendfile  tc01  tcpdump  tracepath01  traceroute01

## net.tirpc_tests（老式 41）
tirpc_authnone_create  tirpc_authsys_create  tirpc_authsys_create_default  tirpc_bottomlevel_clnt_call  tirpc_clnt_control  tirpc_clnt_create  tirpc_clnt_create_timed  tirpc_clnt_destroy  tirpc_clnt_dg_create  tirpc_clnt_pcreateerror  tirpc_clnt_perrno  tirpc_clnt_perror  tirpc_clnt_tli_create  tirpc_clnt_tp_create  tirpc_clnt_tp_create_timed  tirpc_clnt_vc_create  tirpc_expertlevel_clnt_call  tirpc_interlevel_clnt_call  tirpc_rpc_broadcast  tirpc_rpc_broadcast_exp  tirpc_rpc_call  tirpc_rpc_reg  tirpc_rpcb_getaddr  tirpc_rpcb_getmaps  tirpc_rpcb_rmtcall  tirpc_rpcb_set  tirpc_rpcb_unset  tirpc_svc_create  tirpc_svc_destroy  tirpc_svc_dg_create  tirpc_svc_reg  tirpc_svc_tli_create  tirpc_svc_tp_create  tirpc_svc_unreg  tirpc_svc_vc_create  tirpc_svcerr_noproc  tirpc_svcerr_noprog  tirpc_svcerr_progvers  tirpc_svcerr_systemerr  tirpc_svcerr_weakauth  tirpc_toplevel_clnt_call

## net_stress.appl（老式 10）
dns4-stress  dns6-stress  ftp4-download-stress  ftp4-upload-stress  ftp6-download-stress  ftp6-upload-stress  http4-stress  http6-stress  ssh4-stress  ssh6-stress

## net_stress.broken_ip（老式 11）
broken_ip4-checksum  broken_ip4-dstaddr  broken_ip4-fragment  broken_ip4-ihl  broken_ip4-plen  broken_ip4-protcol  broken_ip4-version  broken_ip6-dstaddr  broken_ip6-nexthdr  broken_ip6-plen  broken_ip6-version

## net_stress.interface（老式 25）
if4-addr-adddel_ifconfig  if4-addr-adddel_ip  if4-addr-addlarge_ifconfig  if4-addr-addlarge_ip  if4-addr-change_ifconfig  if4-mtu-change_ifconfig  if4-mtu-change_ip  if4-route-adddel_ip  if4-route-adddel_route  if4-route-addlarge_ip  if4-route-addlarge_route  if4-updown_ifconfig  if4-updown_ip  if6-addr-adddel_ifconfig  if6-addr-adddel_ip  if6-addr-addlarge_ifconfig  if6-addr-addlarge_ip  if6-mtu-change_ifconfig  if6-mtu-change_ip  if6-route-adddel_ip  if6-route-adddel_route  if6-route-addlarge_ip  if6-route-addlarge_route  if6-updown_ifconfig  if6-updown_ip

## net_stress.ipsec_dccp（老式 104）
dccp4_ipsec01  dccp4_ipsec02  dccp4_ipsec03  dccp4_ipsec04  dccp4_ipsec05  dccp4_ipsec06  dccp4_ipsec07  dccp4_ipsec08  dccp4_ipsec09  dccp4_ipsec10  dccp4_ipsec11  dccp4_ipsec12  dccp4_ipsec13  dccp4_ipsec14  dccp4_ipsec15  dccp4_ipsec16  dccp4_ipsec17  dccp4_ipsec18  dccp4_ipsec19  dccp4_ipsec20  dccp4_ipsec21  dccp4_ipsec22  dccp4_ipsec23  dccp4_ipsec24  dccp4_ipsec25  dccp4_ipsec26  dccp4_ipsec27  dccp4_ipsec28  dccp4_ipsec29  dccp4_ipsec30  dccp4_ipsec31  dccp4_ipsec32  dccp4_ipsec33  dccp4_ipsec34  dccp4_ipsec35  dccp4_ipsec36  dccp4_ipsec_vti01  dccp4_ipsec_vti02  dccp4_ipsec_vti04  dccp4_ipsec_vti05  dccp4_ipsec_vti06  dccp4_ipsec_vti07  dccp4_ipsec_vti08  dccp4_ipsec_vti09  dccp4_ipsec_vti10  dccp4_ipsec_vti11  dccp4_ipsec_vti12  dccp4_ipsec_vti13  dccp4_ipsec_vti14  dccp4_ipsec_vti15  dccp4_ipsec_vti16  dccp4_ipsec_vti17  dccp6_ipsec01  dccp6_ipsec02  dccp6_ipsec03  dccp6_ipsec04  dccp6_ipsec05  dccp6_ipsec06  dccp6_ipsec07  dccp6_ipsec08  dccp6_ipsec09  dccp6_ipsec10  dccp6_ipsec11  dccp6_ipsec12  dccp6_ipsec13  dccp6_ipsec14  dccp6_ipsec15  dccp6_ipsec16  dccp6_ipsec17  dccp6_ipsec18  dccp6_ipsec19  dccp6_ipsec20  dccp6_ipsec21  dccp6_ipsec22  dccp6_ipsec23  dccp6_ipsec24  dccp6_ipsec25  dccp6_ipsec26  dccp6_ipsec27  dccp6_ipsec28  dccp6_ipsec29  dccp6_ipsec30  dccp6_ipsec31  dccp6_ipsec32  dccp6_ipsec33  dccp6_ipsec34  dccp6_ipsec35  dccp6_ipsec36  dccp6_ipsec_vti01  dccp6_ipsec_vti02  dccp6_ipsec_vti04  dccp6_ipsec_vti05  dccp6_ipsec_vti06  dccp6_ipsec_vti07  dccp6_ipsec_vti08  dccp6_ipsec_vti09  dccp6_ipsec_vti10  dccp6_ipsec_vti11  dccp6_ipsec_vti12  dccp6_ipsec_vti13  dccp6_ipsec_vti14  dccp6_ipsec_vti15  dccp6_ipsec_vti16  dccp6_ipsec_vti17

## net_stress.ipsec_icmp（老式 86）
icmp4-uni-basic01  icmp4-uni-basic02  icmp4-uni-basic03  icmp4-uni-basic04  icmp4-uni-basic05  icmp4-uni-basic06  icmp4-uni-basic07  icmp4-uni-basic08  icmp4-uni-basic09  icmp4-uni-basic10  icmp4-uni-basic11  icmp4-uni-basic12  icmp4-uni-basic13  icmp4-uni-basic14  icmp4-uni-basic15  icmp4-uni-basic16  icmp4-uni-basic17  icmp4-uni-basic18  icmp4-uni-basic19  icmp4-uni-basic20  icmp4-uni-basic21  icmp4-uni-basic22  icmp4-uni-basic23  icmp4-uni-basic24  icmp4-uni-basic25  icmp4-uni-basic26  icmp4-uni-vti01  icmp4-uni-vti02  icmp4-uni-vti03  icmp4-uni-vti04  icmp4-uni-vti05  icmp4-uni-vti06  icmp4-uni-vti07  icmp4-uni-vti08  icmp4-uni-vti09  icmp4-uni-vti10  icmp4-uni-vti11  icmp4-uni-vti12  icmp4-uni-vti13  icmp4-uni-vti14  icmp4-uni-vti15  icmp4-uni-vti16  icmp4-uni-vti17  icmp6-uni-basic01  icmp6-uni-basic02  icmp6-uni-basic03  icmp6-uni-basic04  icmp6-uni-basic05  icmp6-uni-basic06  icmp6-uni-basic07  icmp6-uni-basic08  icmp6-uni-basic09  icmp6-uni-basic10  icmp6-uni-basic11  icmp6-uni-basic12  icmp6-uni-basic13  icmp6-uni-basic14  icmp6-uni-basic15  icmp6-uni-basic16  icmp6-uni-basic17  icmp6-uni-basic18  icmp6-uni-basic19  icmp6-uni-basic20  icmp6-uni-basic21  icmp6-uni-basic22  icmp6-uni-basic23  icmp6-uni-basic24  icmp6-uni-basic25  icmp6-uni-basic26  icmp6-uni-vti01  icmp6-uni-vti02  icmp6-uni-vti03  icmp6-uni-vti04  icmp6-uni-vti05  icmp6-uni-vti06  icmp6-uni-vti07  icmp6-uni-vti08  icmp6-uni-vti09  icmp6-uni-vti10  icmp6-uni-vti11  icmp6-uni-vti12  icmp6-uni-vti13  icmp6-uni-vti14  icmp6-uni-vti15  icmp6-uni-vti16  icmp6-uni-vti17

## net_stress.ipsec_sctp（老式 104）
sctp4_ipsec01  sctp4_ipsec02  sctp4_ipsec03  sctp4_ipsec04  sctp4_ipsec05  sctp4_ipsec06  sctp4_ipsec07  sctp4_ipsec08  sctp4_ipsec09  sctp4_ipsec10  sctp4_ipsec11  sctp4_ipsec12  sctp4_ipsec13  sctp4_ipsec14  sctp4_ipsec15  sctp4_ipsec16  sctp4_ipsec17  sctp4_ipsec18  sctp4_ipsec19  sctp4_ipsec20  sctp4_ipsec21  sctp4_ipsec22  sctp4_ipsec23  sctp4_ipsec24  sctp4_ipsec25  sctp4_ipsec26  sctp4_ipsec27  sctp4_ipsec28  sctp4_ipsec29  sctp4_ipsec30  sctp4_ipsec31  sctp4_ipsec32  sctp4_ipsec33  sctp4_ipsec34  sctp4_ipsec35  sctp4_ipsec36  sctp4_ipsec_vti01  sctp4_ipsec_vti02  sctp4_ipsec_vti04  sctp4_ipsec_vti05  sctp4_ipsec_vti06  sctp4_ipsec_vti07  sctp4_ipsec_vti08  sctp4_ipsec_vti09  sctp4_ipsec_vti10  sctp4_ipsec_vti11  sctp4_ipsec_vti12  sctp4_ipsec_vti13  sctp4_ipsec_vti14  sctp4_ipsec_vti15  sctp4_ipsec_vti16  sctp4_ipsec_vti17  sctp6_ipsec01  sctp6_ipsec02  sctp6_ipsec03  sctp6_ipsec04  sctp6_ipsec05  sctp6_ipsec06  sctp6_ipsec07  sctp6_ipsec08  sctp6_ipsec09  sctp6_ipsec10  sctp6_ipsec11  sctp6_ipsec12  sctp6_ipsec13  sctp6_ipsec14  sctp6_ipsec15  sctp6_ipsec16  sctp6_ipsec17  sctp6_ipsec18  sctp6_ipsec19  sctp6_ipsec20  sctp6_ipsec21  sctp6_ipsec22  sctp6_ipsec23  sctp6_ipsec24  sctp6_ipsec25  sctp6_ipsec26  sctp6_ipsec27  sctp6_ipsec28  sctp6_ipsec29  sctp6_ipsec30  sctp6_ipsec31  sctp6_ipsec32  sctp6_ipsec33  sctp6_ipsec34  sctp6_ipsec35  sctp6_ipsec36  sctp6_ipsec_vti01  sctp6_ipsec_vti02  sctp6_ipsec_vti04  sctp6_ipsec_vti05  sctp6_ipsec_vti06  sctp6_ipsec_vti07  sctp6_ipsec_vti08  sctp6_ipsec_vti09  sctp6_ipsec_vti10  sctp6_ipsec_vti11  sctp6_ipsec_vti12  sctp6_ipsec_vti13  sctp6_ipsec_vti14  sctp6_ipsec_vti15  sctp6_ipsec_vti16  sctp6_ipsec_vti17

## net_stress.ipsec_tcp（老式 104）
tcp4_ipsec01  tcp4_ipsec02  tcp4_ipsec03  tcp4_ipsec04  tcp4_ipsec05  tcp4_ipsec06  tcp4_ipsec07  tcp4_ipsec08  tcp4_ipsec09  tcp4_ipsec10  tcp4_ipsec11  tcp4_ipsec12  tcp4_ipsec13  tcp4_ipsec14  tcp4_ipsec15  tcp4_ipsec16  tcp4_ipsec17  tcp4_ipsec18  tcp4_ipsec19  tcp4_ipsec20  tcp4_ipsec21  tcp4_ipsec22  tcp4_ipsec23  tcp4_ipsec24  tcp4_ipsec25  tcp4_ipsec26  tcp4_ipsec27  tcp4_ipsec28  tcp4_ipsec29  tcp4_ipsec30  tcp4_ipsec31  tcp4_ipsec32  tcp4_ipsec33  tcp4_ipsec34  tcp4_ipsec35  tcp4_ipsec36  tcp4_ipsec_vti01  tcp4_ipsec_vti02  tcp4_ipsec_vti04  tcp4_ipsec_vti05  tcp4_ipsec_vti06  tcp4_ipsec_vti07  tcp4_ipsec_vti08  tcp4_ipsec_vti09  tcp4_ipsec_vti10  tcp4_ipsec_vti11  tcp4_ipsec_vti12  tcp4_ipsec_vti13  tcp4_ipsec_vti14  tcp4_ipsec_vti15  tcp4_ipsec_vti16  tcp4_ipsec_vti17  tcp6_ipsec01  tcp6_ipsec02  tcp6_ipsec03  tcp6_ipsec04  tcp6_ipsec05  tcp6_ipsec06  tcp6_ipsec07  tcp6_ipsec08  tcp6_ipsec09  tcp6_ipsec10  tcp6_ipsec11  tcp6_ipsec12  tcp6_ipsec13  tcp6_ipsec14  tcp6_ipsec15  tcp6_ipsec16  tcp6_ipsec17  tcp6_ipsec18  tcp6_ipsec19  tcp6_ipsec20  tcp6_ipsec21  tcp6_ipsec22  tcp6_ipsec23  tcp6_ipsec24  tcp6_ipsec25  tcp6_ipsec26  tcp6_ipsec27  tcp6_ipsec28  tcp6_ipsec29  tcp6_ipsec30  tcp6_ipsec31  tcp6_ipsec32  tcp6_ipsec33  tcp6_ipsec34  tcp6_ipsec35  tcp6_ipsec36  tcp6_ipsec_vti01  tcp6_ipsec_vti02  tcp6_ipsec_vti04  tcp6_ipsec_vti05  tcp6_ipsec_vti06  tcp6_ipsec_vti07  tcp6_ipsec_vti08  tcp6_ipsec_vti09  tcp6_ipsec_vti10  tcp6_ipsec_vti11  tcp6_ipsec_vti12  tcp6_ipsec_vti13  tcp6_ipsec_vti14  tcp6_ipsec_vti15  tcp6_ipsec_vti16  tcp6_ipsec_vti17

## net_stress.ipsec_udp（老式 106）
udp4_ipsec01  udp4_ipsec02  udp4_ipsec03  udp4_ipsec04  udp4_ipsec05  udp4_ipsec06  udp4_ipsec07  udp4_ipsec08  udp4_ipsec09  udp4_ipsec10  udp4_ipsec11  udp4_ipsec12  udp4_ipsec13  udp4_ipsec14  udp4_ipsec15  udp4_ipsec16  udp4_ipsec17  udp4_ipsec18  udp4_ipsec19  udp4_ipsec20  udp4_ipsec21  udp4_ipsec22  udp4_ipsec23  udp4_ipsec24  udp4_ipsec25  udp4_ipsec26  udp4_ipsec27  udp4_ipsec28  udp4_ipsec29  udp4_ipsec30  udp4_ipsec31  udp4_ipsec32  udp4_ipsec33  udp4_ipsec34  udp4_ipsec35  udp4_ipsec36  udp4_ipsec_vti01  udp4_ipsec_vti02  udp4_ipsec_vti03  udp4_ipsec_vti04  udp4_ipsec_vti05  udp4_ipsec_vti06  udp4_ipsec_vti07  udp4_ipsec_vti08  udp4_ipsec_vti09  udp4_ipsec_vti10  udp4_ipsec_vti11  udp4_ipsec_vti12  udp4_ipsec_vti13  udp4_ipsec_vti14  udp4_ipsec_vti15  udp4_ipsec_vti16  udp4_ipsec_vti17  udp6_ipsec01  udp6_ipsec02  udp6_ipsec03  udp6_ipsec04  udp6_ipsec05  udp6_ipsec06  udp6_ipsec07  udp6_ipsec08  udp6_ipsec09  udp6_ipsec10  udp6_ipsec11  udp6_ipsec12  udp6_ipsec13  udp6_ipsec14  udp6_ipsec15  udp6_ipsec16  udp6_ipsec17  udp6_ipsec18  udp6_ipsec19  udp6_ipsec20  udp6_ipsec21  udp6_ipsec22  udp6_ipsec23  udp6_ipsec24  udp6_ipsec25  udp6_ipsec26  udp6_ipsec27  udp6_ipsec28  udp6_ipsec29  udp6_ipsec30  udp6_ipsec31  udp6_ipsec32  udp6_ipsec33  udp6_ipsec34  udp6_ipsec35  udp6_ipsec36  udp6_ipsec_vti01  udp6_ipsec_vti02  udp6_ipsec_vti03  udp6_ipsec_vti04  udp6_ipsec_vti05  udp6_ipsec_vti06  udp6_ipsec_vti07  udp6_ipsec_vti08  udp6_ipsec_vti09  udp6_ipsec_vti10  udp6_ipsec_vti11  udp6_ipsec_vti12  udp6_ipsec_vti13  udp6_ipsec_vti14  udp6_ipsec_vti15  udp6_ipsec_vti16  udp6_ipsec_vti17

## net_stress.multicast（老式 24）
mcast4-group-multiple-socket  mcast4-group-same-group  mcast4-group-single-socket  mcast4-group-source-filter  mcast4-pktfld01  mcast4-pktfld02  mcast4-queryfld01  mcast4-queryfld02  mcast4-queryfld03  mcast4-queryfld04  mcast4-queryfld05  mcast4-queryfld06  mcast6-group-multiple-socket  mcast6-group-same-group  mcast6-group-single-socket  mcast6-group-source-filter  mcast6-pktfld01  mcast6-pktfld02  mcast6-queryfld01  mcast6-queryfld02  mcast6-queryfld03  mcast6-queryfld04  mcast6-queryfld05  mcast6-queryfld06

## net_stress.route（老式 14）
route4-change-dst  route4-change-gw  route4-change-if  route4-change-netlink-dst  route4-change-netlink-gw  route4-change-netlink-if  route4-redirect  route6-change-dst  route6-change-gw  route6-change-if  route6-change-netlink-dst  route6-change-netlink-gw  route6-change-netlink-if  route6-redirect

## nptl（老式 1）
nptl01

## numa（老式 12）
migrate_pages01  move_pages01  move_pages02  move_pages03  move_pages04  move_pages05  move_pages06  move_pages07  move_pages09  move_pages10  move_pages11  numa_testcases

## power_management_tests（老式 5）
runpwtests01  runpwtests02  runpwtests03  runpwtests04  runpwtests06

## power_management_tests_exclusive（老式 5）
runpwtests_exclusive01  runpwtests_exclusive02  runpwtests_exclusive03  runpwtests_exclusive04  runpwtests_exclusive05

## pty（老式 3）
hangup01  ptem01  pty01

## s390x_tests（老式 1）
vmcp

## sched（老式 9）
hackbench01  hackbench02  pth_str01  pth_str02  pth_str03  sched_cli_serv  sched_stress  time-schedule01  trace_sched01

## scsi_debug.part1（老式 125）
gf101  gf102  gf103  gf104  gf105  gf106  gf107  gf108  gf109  gf110  gf111  gf112  gf113  gf114  gf115  gf116  gf117  gf118  gf119  gf120  gf121  gf122  gf123  gf124  gf125  gf126  gf127  gf128  gf129  gf130  gf201  gf202  gf203  gf204  gf205  gf206  gf207  gf208  gf209  gf210  gf211  gf212  gf213  gf214  gf215  gf216  gf217  gf218  gf219  gf220  gf221  gf222  gf223  gf224  gf225  gf226  gf227  gf228  gf229  gf230  gf301  gf302  gf303  gf304  gf305  gf306  gf307  gf308  gf309  gf310  gf311  gf312  gf313  gf314  gf315  gf316  gf317  gf318  gf319  gf320  gf321  gf322  gf323  gf324  gf325  gf326  gf327  gf328  gf329  gf330  gf701  gf702  gf703  gf704  gf705  gf706  gf707  gf708  gf709  gf710  gf711  gf712  gf713  gf714  gf715  gf716  gf717  gf718  gf719  gf720  gf721  gf722  gf723  gf724  gf725  gf726  gf727  gf728  gf729  gf730  rwtest01  rwtest02  rwtest03  rwtest04  rwtest05

## smack（老式 10）
smack_file_access  smack_set_ambient  smack_set_cipso  smack_set_current  smack_set_direct  smack_set_doi  smack_set_load  smack_set_netlabel  smack_set_onlycap  smack_set_socket_labels

## smoketest（老式 6）
df01_sh  macsec02  ping602  shell_test01  stat04  symlink01

## syscalls（老式 229）
clone02  connect01  epoll01  exit01  fallocate01  fallocate02  fchownat01  fchownat02  fcntl01  fcntl01_64  fcntl07  fcntl07_64  fcntl09  fcntl09_64  fcntl10  fcntl10_64  fcntl11  fcntl11_64  fcntl14  fcntl14_64  fcntl16  fcntl16_64  fcntl17  fcntl17_64  fcntl18  fcntl18_64  fcntl19  fcntl19_64  fcntl20  fcntl20_64  fcntl21  fcntl21_64  fcntl22  fcntl22_64  fcntl23  fcntl23_64  fcntl24  fcntl24_64  fcntl25  fcntl25_64  fcntl26  fcntl26_64  fcntl31  fcntl31_64  fcntl32  fcntl32_64  fdatasync01  fdatasync02  fmtmsg01  fork05  fork06  fork09  fork11  fstatat01  futimesat01  get_robust_list01  getgroups01  getgroups01_16  getgroups03  getgroups03_16  getresgid01  getresgid01_16  getresgid02  getresgid02_16  getresgid03  getresgid03_16  getresuid01  getresuid01_16  getresuid02  getresuid02_16  getresuid03  getresuid03_16  getrusage04  kill02  kill07  kill08  kill09  kill10  kill12  lchown01  lchown01_16  lchown02  lchown02_16  lchown03  lchown03_16  link01  linkat01  linkat02  listen01  migrate_pages01  mincore01  mkdirat01  mknod03  mknod04  mknod05  mknod06  mknod07  mknod08  mknodat01  mknodat02  mlockall01  mlockall02  mlockall03  mmap01  mmap03  mmap14  modify_ldt01  modify_ldt02  modify_ldt03  move_pages01  move_pages02  move_pages03  move_pages04  move_pages05  move_pages06  move_pages07  move_pages09  move_pages10  move_pages11  mprotect01  mprotect02  mprotect03  mprotect04  mremap01  mremap02  mremap03  mremap04  mremap05  msync01  msync02  msync03  munmap01  munmap02  munmap03  newuname01  nftw01  nftw6401  open12  open13  open14  openat02  openat03  pause02  pause03  perf_event_open01  pipe04  pipe05  pipe09  process_vm_readv01  process_vm_writev01  profil01  prot_hsymlinks  ptrace04  ptrace05  ptrace06  qmm01  recv01  recvfrom01  remap_file_pages01  removexattr01  removexattr02  rename11  rename14  renameat01  renameat201  renameat202  rt_sigaction01  rt_sigaction02  rt_sigaction03  rt_sigprocmask01  rt_sigprocmask02  sched_getattr01  sched_getattr02  sched_setattr01  sched_yield01  semctl06  semop05  send01  sendmsg01  sendmsg02  sendto01  set_robust_list01  set_thread_area01  set_tid_address01  setfsgid03  setfsgid03_16  setfsuid04  setfsuid04_16  sethostname01  sethostname02  sethostname03  setpgid01  setpgrp01  setresgid01  setresgid01_16  setresgid04  setresgid04_16  setrlimit01  setsid01  sgetmask01  sigaction01  sigaction02  sigaltstack01  signal06  signalfd01  signalfd4_01  signalfd4_02  sigprocmask01  sigrelse01  sockioctl01  ssetmask01  stat04  stat04_64  string01  switch01  symlink01  symlink03  symlinkat01  sysconf01  sysinfo01  sysinfo02  ulimit01  umount2_01  unlink01  vfork01  vfork02  writev02  writev05  writev06

## syscalls-ipc（老式 1）
semctl06

## tpm_tools（老式 12）
tpm01  tpm02  tpm03  tpm04  tpm05  tpm06  tpm07  tpmtoken01  tpmtoken02  tpmtoken03  tpmtoken04  tpmtoken05

## tracing（老式 9）
dynamic_debug01  ftrace-stress-test  ftrace_regression01  ftrace_regression02  pt_disable_branch  pt_ex_kernel  pt_ex_user  pt_full_trace_basic  pt_snapshot_trace_basic


---
# 新式(可计分)用例清单 —— 按测试集

## can（新式 3）
can_bcm01  can_filter  can_rcv_own_msgs

## containers（新式 40）
clock_gettime03  clock_nanosleep03  mountns01  mountns02  mountns03  mountns04  mqns_01  mqns_02  msg_comm  netns_netlink  pidns01  pidns02  pidns03  pidns04  pidns05  pidns06  pidns10  pidns12  pidns13  pidns16  pidns17  pidns20  pidns30  pidns31  pidns32  sem_comm  shm_comm  sysinfo03  timens01  timerfd04  userns01  userns02  userns03  userns04  userns05  userns06  userns07  userns08  utsname01  utsname02

## controllers（新式 9）
cgroup_core01  cgroup_core02  cgroup_core03  io_control01  memcg_test_3  memcontrol01  memcontrol02  memcontrol03  memcontrol04

## crypto（新式 10）
af_alg01  af_alg02  af_alg03  af_alg04  af_alg05  af_alg06  af_alg07  crypto_user01  crypto_user02  pcrypt_aead01

## cve（新式 11）
cve-2014-0196  cve-2015-3290  cve-2016-10044  cve-2016-7042  cve-2016-7117  cve-2017-16939  cve-2017-17052  cve-2017-17053  cve-2017-2618  cve-2017-2671  cve-2022-4378

## fs（新式 2）
fs_fill  squashfs01

## hugetlb（新式 48）
hugefallocate01  hugefallocate02  hugefork01  hugefork02  hugemmap01  hugemmap02  hugemmap04  hugemmap05  hugemmap06  hugemmap07  hugemmap08  hugemmap09  hugemmap10  hugemmap11  hugemmap12  hugemmap13  hugemmap14  hugemmap15  hugemmap16  hugemmap17  hugemmap18  hugemmap19  hugemmap20  hugemmap21  hugemmap22  hugemmap23  hugemmap24  hugemmap25  hugemmap26  hugemmap27  hugemmap28  hugemmap29  hugemmap30  hugemmap31  hugemmap32  hugeshmat01  hugeshmat02  hugeshmat03  hugeshmat04  hugeshmat05  hugeshmctl01  hugeshmctl02  hugeshmctl03  hugeshmdt01  hugeshmget01  hugeshmget02  hugeshmget03  hugeshmget05

## irq（新式 1）
irqbalance01

## kernel_misc（新式 5）
aslr01  kmsg01  rtc02  umip_basic_test  zram03

## kvm（新式 5）
kvm_pagefault01  kvm_svm01  kvm_svm02  kvm_svm03  kvm_svm04

## ltp-aiodio.part4（新式 1）
aio02

## mm（新式 24）
cpuset01  kallsyms  ksm01  ksm02  ksm03  ksm04  ksm05  ksm06  ksm07  max_map_count  min_free_kbytes  mmapstress01  mmapstress04  mtest01  oom01  oom02  oom03  oom04  oom05  swapping01  thp01  thp02  thp03  thp04

## net.features（新式 1）
fanout01

## net.ipv6_lib（新式 4）
asapi_02  getaddrinfo_01  in6_01  in6_02

## numa（新式 8）
migrate_pages02  migrate_pages03  move_pages12  set_mempolicy01  set_mempolicy02  set_mempolicy03  set_mempolicy04  set_mempolicy05

## pty（新式 6）
pty02  pty03  pty04  pty05  pty06  pty07

## sched（新式 4）
autogroup01  cfs_bandwidth01  proc_sched_rt01  starvation

## smoketest（新式 9）
access01  chdir01  fork01  rename01A  splice02  time01  utime07  wait02  write01

## syscalls（新式 1182）
abort01  accept01  accept02  accept03  accept4_01  access01  access02  access03  access04  acct01  acct02  add_key01  add_key02  add_key03  add_key04  add_key05  adjtimex01  adjtimex02  adjtimex03  alarm02  alarm03  alarm05  alarm06  alarm07  arch_prctl01  bind01  bind02  bind03  bind04  bind05  bind06  bpf_map01  bpf_prog01  bpf_prog02  bpf_prog03  bpf_prog04  bpf_prog05  bpf_prog06  bpf_prog07  brk01  brk02  cacheflush01  capget01  capget02  capset01  capset02  capset03  capset04  chdir01  chdir01A  chdir04  chmod01  chmod01A  chmod03  chmod05  chmod06  chmod07  chown01  chown01_16  chown02  chown02_16  chown03  chown03_16  chown04  chown04_16  chown05  chown05_16  chroot01  chroot02  chroot03  chroot04  clock_adjtime01  clock_adjtime02  clock_getres01  clock_gettime01  clock_gettime02  clock_gettime03  clock_gettime04  clock_nanosleep01  clock_nanosleep02  clock_nanosleep03  clock_nanosleep04  clock_settime01  clock_settime02  clock_settime03  clone01  clone03  clone04  clone05  clone06  clone07  clone08  clone09  clone301  clone302  clone303  close01  close02  close_range01  close_range02  confstr01  connect02  copy_file_range01  copy_file_range02  copy_file_range03  creat01  creat03  creat04  creat05  creat06  creat07  creat08  creat09  delete_module01  delete_module02  delete_module03  dirtyc0w  dirtyc0w_shmem  dirtypipe  dup01  dup02  dup03  dup04  dup05  dup06  dup07  dup201  dup202  dup203  dup204  dup205  dup206  dup207  dup3_01  dup3_02  epoll_create01  epoll_create02  epoll_create1_01  epoll_create1_02  epoll_ctl01  epoll_ctl02  epoll_ctl03  epoll_ctl04  epoll_ctl05  epoll_pwait01  epoll_pwait02  epoll_pwait03  epoll_pwait04  epoll_pwait05  epoll_wait01  epoll_wait02  epoll_wait03  epoll_wait04  epoll_wait05  epoll_wait06  epoll_wait07  eventfd01  eventfd02  eventfd03  eventfd04  eventfd05  eventfd06  eventfd2_01  eventfd2_02  eventfd2_03  execl01  execle01  execlp01  execv01  execve01  execve02  execve03  execve04  execve05  execve06  execveat01  execveat02  execveat03  execvp01  exit02  exit_group01  faccessat01  faccessat02  faccessat201  faccessat202  fallocate03  fallocate04  fallocate05  fallocate06  fanotify01  fanotify02  fanotify03  fanotify04  fanotify05  fanotify06  fanotify07  fanotify08  fanotify09  fanotify10  fanotify11  fanotify12  fanotify13  fanotify14  fanotify15  fanotify16  fanotify17  fanotify18  fanotify19  fanotify20  fanotify21  fanotify22  fanotify23  fchdir01  fchdir02  fchdir03  fchmod01  fchmod02  fchmod03  fchmod04  fchmod05  fchmod06  fchmodat01  fchmodat02  fchown01  fchown01_16  fchown02  fchown02_16  fchown03  fchown03_16  fchown04  fchown04_16  fchown05  fchown05_16  fcntl02  fcntl02_64  fcntl03  fcntl03_64  fcntl04  fcntl04_64  fcntl05  fcntl05_64  fcntl08  fcntl08_64  fcntl12  fcntl12_64  fcntl13  fcntl13_64  fcntl15  fcntl15_64  fcntl27  fcntl27_64  fcntl29  fcntl29_64  fcntl30  fcntl30_64  fcntl33  fcntl33_64  fcntl34  fcntl34_64  fcntl35  fcntl35_64  fcntl36  fcntl36_64  fcntl37  fcntl37_64  fcntl38  fcntl38_64  fcntl39  fcntl39_64  fdatasync03  fgetxattr01  fgetxattr02  fgetxattr03  finit_module01  finit_module02  flistxattr01  flistxattr02  flistxattr03  flock01  flock02  flock03  flock04  flock06  fork01  fork03  fork04  fork07  fork08  fork10  fork13  fork14  fpathconf01  fremovexattr01  fremovexattr02  fsconfig01  fsconfig02  fsconfig03  fsetxattr01  fsetxattr02  fsmount01  fsmount02  fsopen01  fsopen02  fspick01  fspick02  fstat02  fstat02_64  fstat03  fstat03_64  fstatfs01  fstatfs01_64  fstatfs02  fstatfs02_64  fsync01  fsync02  fsync03  fsync04  ftruncate01  ftruncate01_64  ftruncate03  ftruncate03_64  ftruncate04  ftruncate04_64  futex_cmp_requeue01  futex_cmp_requeue02  futex_wait01  futex_wait02  futex_wait03  futex_wait04  futex_wait05  futex_wait_bitset01  futex_waitv01  futex_waitv02  futex_waitv03  futex_wake01  futex_wake02  futex_wake03  futex_wake04  get_mempolicy01  get_mempolicy02  getcontext01  getcpu01  getcwd01  getcwd02  getcwd03  getcwd04  getdents01  getdents02  getdomainname01  getegid01  getegid01_16  getegid02  getegid02_16  geteuid01  geteuid01_16  geteuid02  geteuid02_16  getgid01  getgid01_16  getgid03  getgid03_16  gethostbyname_r01  gethostid01  gethostname01  gethostname02  getitimer01  getitimer02  getpagesize01  getpeername01  getpgid01  getpgid02  getpgrp01  getpid01  getpid02  getppid01  getppid02  getpriority01  getpriority02  getrandom01  getrandom02  getrandom03  getrandom04  getrandom05  getrlimit01  getrlimit02  getrlimit03  getrusage01  getrusage02  getrusage03  getsid01  getsid02  getsockname01  getsockopt01  getsockopt02  gettid01  gettid02  gettimeofday01  gettimeofday02  getuid01  getuid01_16  getuid03  getuid03_16  getxattr01  getxattr02  getxattr03  getxattr04  getxattr05  init_module01  init_module02  inotify01  inotify02  inotify03  inotify04  inotify05  inotify06  inotify07  inotify08  inotify09  inotify10  inotify11  inotify12  inotify_init1_01  inotify_init1_02  io_cancel01  io_cancel02  io_destroy01  io_destroy02  io_getevents01  io_getevents02  io_pgetevents01  io_pgetevents02  io_setup01  io_setup02  io_submit01  io_submit02  io_submit03  io_uring01  io_uring02  ioctl01  ioctl02  ioctl03  ioctl04  ioctl05  ioctl06  ioctl07  ioctl08  ioctl09  ioctl_loop01  ioctl_loop02  ioctl_loop03  ioctl_loop04  ioctl_loop05  ioctl_loop06  ioctl_loop07  ioctl_ns01  ioctl_ns02  ioctl_ns03  ioctl_ns04  ioctl_ns05  ioctl_ns06  ioctl_ns07  ioctl_sg01  ioperm01  ioperm02  iopl01  iopl02  ioprio_get01  ioprio_set01  ioprio_set02  ioprio_set03  kcmp01  kcmp02  kcmp03  keyctl01  keyctl02  keyctl03  keyctl04  keyctl05  keyctl06  keyctl07  keyctl08  keyctl09  kill03  kill05  kill06  kill11  kill13  leapsec01  lgetxattr01  lgetxattr02  link02  link04  link05  link08  listxattr01  listxattr02  listxattr03  llistxattr01  llistxattr02  llistxattr03  llseek01  llseek02  llseek03  lremovexattr01  lseek01  lseek02  lseek07  lseek11  lstat01  lstat01A  lstat01A_64  lstat01_64  lstat02  lstat02_64  madvise01  madvise02  madvise03  madvise05  madvise06  madvise07  madvise08  madvise09  madvise10  madvise11  mallinfo02  mallinfo2_01  mallopt01  mbind01  mbind02  mbind03  mbind04  membarrier01  memcmp01  memcpy01  memfd_create01  memfd_create02  memfd_create03  memfd_create04  memset01  migrate_pages02  migrate_pages03  mincore02  mincore03  mincore04  mkdir02  mkdir03  mkdir04  mkdir05  mkdir09  mkdirat02  mknod01  mknod02  mknod09  mlock01  mlock02  mlock03  mlock04  mlock05  mlock201  mlock202  mlock203  mmap02  mmap04  mmap05  mmap06  mmap08  mmap09  mmap12  mmap13  mmap15  mmap16  mmap17  mmap18  mmap19  mmap20  mount01  mount02  mount03  mount04  mount05  mount06  mount07  mount_setattr01  move_mount01  move_mount02  move_pages12  mprotect05  mq_notify01  mq_notify02  mq_notify03  mq_open01  mq_timedreceive01  mq_timedsend01  mq_unlink01  mremap06  msgctl01  msgctl02  msgctl03  msgctl04  msgctl05  msgctl06  msgctl12  msgget01  msgget02  msgget03  msgget04  msgget05  msgrcv01  msgrcv02  msgrcv03  msgrcv05  msgrcv06  msgrcv07  msgrcv08  msgsnd01  msgsnd02  msgsnd05  msgsnd06  msgstress01  msync04  munlock01  munlock02  munlockall01  name_to_handle_at01  name_to_handle_at02  nanosleep01  nanosleep02  nanosleep04  nice01  nice02  nice03  nice04  nice05  open01  open01A  open02  open03  open04  open06  open07  open08  open09  open10  open11  open_by_handle_at01  open_by_handle_at02  open_tree01  open_tree02  openat01  openat04  openat201  openat202  openat203  pathconf01  pathconf02  pause01  perf_event_open02  perf_event_open03  personality01  personality02  pidfd_getfd01  pidfd_getfd02  pidfd_open01  pidfd_open02  pidfd_open03  pidfd_open04  pidfd_send_signal01  pidfd_send_signal02  pidfd_send_signal03  pipe01  pipe02  pipe03  pipe06  pipe07  pipe08  pipe10  pipe11  pipe12  pipe13  pipe14  pipe15  pipe2_01  pipe2_02  pipe2_04  pivot_root01  pkey01  poll01  poll02  posix_fadvise01  posix_fadvise01_64  posix_fadvise02  posix_fadvise02_64  posix_fadvise03  posix_fadvise03_64  posix_fadvise04  posix_fadvise04_64  ppoll01  prctl01  prctl02  prctl03  prctl04  prctl05  prctl06  prctl07  prctl08  prctl09  prctl10  pread01  pread01_64  pread02  pread02_64  preadv01  preadv01_64  preadv02  preadv02_64  preadv03  preadv03_64  preadv201  preadv201_64  preadv202  preadv202_64  preadv203  preadv203_64  process_madvise01  process_vm_readv02  process_vm_readv03  process_vm_writev02  pselect01  pselect01_64  pselect02  pselect02_64  pselect03  pselect03_64  ptrace01  ptrace02  ptrace03  ptrace07  ptrace08  ptrace09  ptrace10  ptrace11  pwrite01  pwrite01_64  pwrite02  pwrite02_64  pwrite03  pwrite03_64  pwrite04  pwrite04_64  pwritev01  pwritev01_64  pwritev02  pwritev02_64  pwritev03  pwritev03_64  pwritev201  pwritev201_64  pwritev202  pwritev202_64  quotactl01  quotactl02  quotactl03  quotactl04  quotactl05  quotactl06  quotactl07  quotactl08  quotactl09  read01  read02  read03  read04  readahead01  readahead02  readdir01  readdir21  readlink01  readlink01A  readlink03  readlinkat01  readlinkat02  readv01  readv02  realpath01  reboot01  reboot02  recvmmsg01  recvmsg01  recvmsg02  recvmsg03  remap_file_pages02  rename01  rename01A  rename03  rename04  rename05  rename06  rename07  rename08  rename09  rename10  rename12  rename13  request_key01  request_key02  request_key03  request_key04  request_key05  rmdir01  rmdir02  rmdir03  rmdir03A  rt_sigqueueinfo01  rt_sigsuspend01  rt_sigtimedwait01  rt_tgsigqueueinfo01  sbrk01  sbrk02  sbrk03  sched_get_priority_max01  sched_get_priority_max02  sched_get_priority_min01  sched_get_priority_min02  sched_getaffinity01  sched_getparam01  sched_getparam03  sched_getscheduler01  sched_getscheduler02  sched_rr_get_interval01  sched_rr_get_interval02  sched_rr_get_interval03  sched_setaffinity01  sched_setparam01  sched_setparam02  sched_setparam03  sched_setparam04  sched_setparam05  sched_setscheduler01  sched_setscheduler02  sched_setscheduler03  sched_setscheduler04  select01  select02  select03  select04  semctl01  semctl02  semctl03  semctl04  semctl05  semctl07  semctl08  semctl09  semget01  semget02  semget05  semop01  semop02  semop03  semop04  send02  sendfile02  sendfile02_64  sendfile03  sendfile03_64  sendfile04  sendfile04_64  sendfile05  sendfile05_64  sendfile06  sendfile06_64  sendfile07  sendfile07_64  sendfile08  sendfile08_64  sendfile09  sendfile09_64  sendmmsg01  sendmmsg02  sendmsg03  sendto02  sendto03  set_mempolicy01  set_mempolicy02  set_mempolicy03  set_mempolicy04  setdomainname01  setdomainname02  setdomainname03  setegid01  setegid02  setfsgid01  setfsgid01_16  setfsgid02  setfsgid02_16  setfsuid01  setfsuid01_16  setfsuid02  setfsuid02_16  setfsuid03  setfsuid03_16  setgid01  setgid01_16  setgid02  setgid02_16  setgid03  setgid03_16  setgroups01  setgroups01_16  setgroups02  setgroups02_16  setgroups03  setgroups03_16  setitimer01  setitimer02  setns01  setns02  setpgid02  setpgid03  setpgrp02  setpriority01  setpriority02  setregid01  setregid01_16  setregid02  setregid02_16  setregid03  setregid03_16  setregid04  setregid04_16  setresgid02  setresgid02_16  setresgid03  setresgid03_16  setresuid01  setresuid01_16  setresuid02  setresuid02_16  setresuid03  setresuid03_16  setresuid04  setresuid04_16  setresuid05  setresuid05_16  setreuid01  setreuid01_16  setreuid02  setreuid02_16  setreuid03  setreuid03_16  setreuid04  setreuid04_16  setreuid05  setreuid05_16  setreuid06  setreuid06_16  setreuid07  setreuid07_16  setrlimit02  setrlimit03  setrlimit04  setrlimit05  setrlimit06  setsockopt01  setsockopt02  setsockopt03  setsockopt04  setsockopt05  setsockopt06  setsockopt07  setsockopt08  setsockopt09  setsockopt10  settimeofday01  settimeofday02  setuid01  setuid01_16  setuid03  setuid03_16  setuid04  setuid04_16  setxattr01  setxattr02  setxattr03  shmat01  shmat02  shmat03  shmat04  shmctl01  shmctl02  shmctl03  shmctl04  shmctl05  shmctl06  shmctl07  shmctl08  shmdt01  shmdt02  shmget02  shmget03  shmget04  shmget05  shmget06  sigaltstack02  sighold02  signal01  signal02  signal03  signal04  signal05  sigpending02  sigsuspend01  sigtimedwait01  sigwait01  sigwaitinfo01  socket01  socket02  socketcall01  socketcall02  socketcall03  socketpair01  socketpair02  splice01  splice02  splice03  splice04  splice05  splice06  splice07  splice08  splice09  stat01  stat01_64  stat02  stat02_64  stat03  stat03_64  statfs01  statfs01_64  statfs02  statfs02_64  statfs03  statfs03_64  statvfs01  statvfs02  statx01  statx02  statx03  statx04  statx05  statx06  statx07  statx08  statx09  statx10  statx11  statx12  stime01  stime02  swapoff01  swapoff02  swapon01  swapon02  swapon03  symlink02  symlink04  sync01  sync_file_range01  sync_file_range02  syncfs01  syscall01  sysctl01  sysctl03  sysctl04  sysfs01  sysfs02  sysfs03  sysfs04  sysfs05  sysinfo03  syslog11  syslog12  tee01  tee02  tgkill01  tgkill02  tgkill03  time01  timer_create01  timer_create02  timer_create03  timer_delete01  timer_delete02  timer_getoverrun01  timer_gettime01  timer_settime01  timer_settime02  timer_settime03  timerfd01  timerfd02  timerfd04  timerfd_create01  timerfd_gettime01  timerfd_settime01  timerfd_settime02  times01  times03  tkill01  tkill02  truncate02  truncate02_64  truncate03  truncate03_64  umask01  umount01  umount02  umount03  umount2_02  uname01  uname02  uname04  unlink05  unlink07  unlink08  unlink09  unlinkat01  unshare01  unshare02  userfaultfd01  ustat01  ustat02  utime01  utime02  utime03  utime04  utime05  utime06  utime07  utimensat01  utimes01  vhangup01  vhangup02  vmsplice01  vmsplice02  vmsplice03  vmsplice04  wait01  wait02  wait401  wait402  wait403  waitid01  waitid02  waitid03  waitid04  waitid05  waitid06  waitid07  waitid08  waitid09  waitid10  waitid11  waitpid01  waitpid03  waitpid04  waitpid06  waitpid07  waitpid08  waitpid09  waitpid10  waitpid11  waitpid12  waitpid13  write01  write02  write03  write04  write05  write06  writev01  writev03  writev07

## syscalls-ipc（新式 56）
msgctl01  msgctl02  msgctl03  msgctl04  msgctl05  msgctl06  msgctl12  msgget01  msgget02  msgget03  msgget04  msgget05  msgrcv01  msgrcv02  msgrcv03  msgrcv05  msgrcv06  msgrcv07  msgrcv08  msgsnd01  msgsnd02  msgsnd05  msgsnd06  msgstress01  semctl01  semctl02  semctl03  semctl04  semctl05  semctl07  semctl08  semctl09  semget01  semget02  semget05  semop01  semop02  semop03  shmat01  shmat02  shmat04  shmctl01  shmctl02  shmctl03  shmctl04  shmctl05  shmctl06  shmctl07  shmctl08  shmdt01  shmdt02  shmget02  shmget03  shmget04  shmget05  shmget06

## uevent（新式 3）
uevent01  uevent02  uevent03

## watchqueue（新式 9）
wqueue01  wqueue02  wqueue03  wqueue04  wqueue05  wqueue06  wqueue07  wqueue08  wqueue09

