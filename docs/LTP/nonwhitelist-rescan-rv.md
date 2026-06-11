# rv 非白名单 LTP 扫描（5 个一组，组 60s 超时杀 hang，无 -I 单次执行=计分口径）

逐组追加。passed = Summary.passed（judge 口径，>0 即可加入白名单候选）。status: ok / HANG（卡住）/ notrun（qemu 早死未跑到，需复扫）。

| case | passed | status |
|---|---|---|

<!-- group 0 (change_password.sh,chdir01,clock_gettime01,clock_gettime04,clone02) rc=0 -->
| change_password.sh | 0 | ok |
| chdir01 | 0 | ok |
| clone02 | 0 | ok |
| clock_gettime01 | - | notrun |
| clock_gettime04 | - | notrun |

<!-- group 5 (clone09,clone301,connect01,cp_tests.sh,cpuacct_task) rc=0 -->
| clone09 | 0 | ok |
| clone301 | 0 | ok |
| connect01 | 0 | ok |
| cp_tests.sh | 0 | ok |
| cpuacct_task | 0 | ok |

<!-- group 10 (cpufreq_boost,cpuhotplug_do_kcompile_loop,cpuhotplug_do_spin_loop,creat07_child,creat09) rc=124 -->
| cpufreq_boost | 0 | ok |
| cpuhotplug_do_kcompile_loop | 0 | ok |
| cpuhotplug_do_spin_loop | - | HANG |
| creat07_child | - | notrun |
| creat09 | - | notrun |
<!-- group 10 超时(hang)->已恢复镜像 -->

<!-- group 15 (cve-2017-16939,dirtyc0w_shmem,du01.sh,dynamic_debug01.sh,ebizzy) rc=124 -->
| cve-2017-16939 | 0 | ok |
| du01.sh | - | HANG |
| dirtyc0w_shmem | - | notrun |
| dynamic_debug01.sh | - | notrun |
| ebizzy | - | notrun |
<!-- group 15 超时(hang)->已恢复镜像 -->

<!-- group 20 (endian_switch01,epoll-ltp,epoll_pwait01,float_iperb,force_erase.sh) rc=124 -->
| endian_switch01 | 0 | ok |
| epoll-ltp | 0 | ok |
| epoll_pwait01 | 0 | ok |
| float_iperb | - | HANG |
| force_erase.sh | - | notrun |
<!-- group 20 超时(hang)->已恢复镜像 -->

<!-- group 25 (fork05,fork09,fork14,fork_freeze.sh,fork_procs) rc=0 -->
| fork05 | 0 | ok |
| fork09 | 0 | ok |
| fork_freeze.sh | 0 | ok |
| fork_procs | 1 | ok |
| fork14 | - | notrun |

<!-- group 30 (fs_bind02.sh,fs_bind04.sh,fs_bind05.sh,fs_bind06.sh,fs_bind07-2.sh) rc=124 -->
| fs_bind02.sh | - | HANG |
| fs_bind04.sh | - | notrun |
| fs_bind05.sh | - | notrun |
| fs_bind06.sh | - | notrun |
| fs_bind07-2.sh | - | notrun |
<!-- group 30 超时(hang)->已恢复镜像 -->

<!-- group 35 (fs_bind08.sh,fs_bind09.sh,fs_bind10.sh,fs_bind11.sh,fs_bind13.sh) rc=124 -->
| fs_bind08.sh | - | HANG |
| fs_bind09.sh | - | notrun |
| fs_bind10.sh | - | notrun |
| fs_bind11.sh | - | notrun |
| fs_bind13.sh | - | notrun |
<!-- group 35 超时(hang)->已恢复镜像 -->

<!-- group 40 (fs_bind14.sh,fs_bind15.sh,fs_bind16.sh,fs_bind18.sh,fs_bind19.sh) rc=124 -->
| fs_bind14.sh | - | HANG |
| fs_bind15.sh | - | notrun |
| fs_bind16.sh | - | notrun |
| fs_bind18.sh | - | notrun |
| fs_bind19.sh | - | notrun |
<!-- group 40 超时(hang)->已恢复镜像 -->

<!-- group 45 (fs_bind20.sh,fs_bind21.sh,fs_bind23.sh,fs_bind24.sh,fs_bind_cloneNS01.sh) rc=124 -->
| fs_bind20.sh | - | HANG |
| fs_bind21.sh | - | notrun |
| fs_bind23.sh | - | notrun |
| fs_bind24.sh | - | notrun |
| fs_bind_cloneNS01.sh | - | notrun |
<!-- group 45 超时(hang)->已恢复镜像 -->

<!-- group 50 (fs_bind_cloneNS02.sh,fs_bind_cloneNS04.sh,fs_bind_cloneNS05.sh,fs_bind_cloneNS06.sh,fs_bind_cloneNS07.sh) rc=124 -->
| fs_bind_cloneNS02.sh | - | HANG |
| fs_bind_cloneNS04.sh | - | notrun |
| fs_bind_cloneNS05.sh | - | notrun |
| fs_bind_cloneNS06.sh | - | notrun |
| fs_bind_cloneNS07.sh | - | notrun |
<!-- group 50 超时(hang)->已恢复镜像 -->

<!-- group 55 (fs_bind_move02.sh,fs_bind_move03.sh,fs_bind_move04.sh,fs_bind_move06.sh,fs_bind_move07.sh) rc=124 -->
| fs_bind_move02.sh | - | HANG |
| fs_bind_move03.sh | - | notrun |
| fs_bind_move04.sh | - | notrun |
| fs_bind_move06.sh | - | notrun |
| fs_bind_move07.sh | - | notrun |
<!-- group 55 超时(hang)->已恢复镜像 -->

<!-- group 60 (fs_bind_move08.sh,fs_bind_move09.sh,fs_bind_move11.sh,fs_bind_move12.sh,fs_bind_move13.sh) rc=124 -->
| fs_bind_move08.sh | - | HANG |
| fs_bind_move09.sh | - | notrun |
| fs_bind_move11.sh | - | notrun |
| fs_bind_move12.sh | - | notrun |
| fs_bind_move13.sh | - | notrun |
<!-- group 60 超时(hang)->已恢复镜像 -->

<!-- group 65 (fs_bind_move14.sh,fs_bind_move16.sh,fs_bind_move17.sh,fs_bind_move18.sh,fs_bind_move19.sh) rc=124 -->
| fs_bind_move14.sh | - | HANG |
| fs_bind_move16.sh | - | notrun |
| fs_bind_move17.sh | - | notrun |
| fs_bind_move18.sh | - | notrun |
| fs_bind_move19.sh | - | notrun |
<!-- group 65 超时(hang)->已恢复镜像 -->

<!-- group 70 (fs_bind_move21.sh,fs_bind_move22.sh,fs_bind_rbind01.sh,fs_bind_rbind02.sh,fs_bind_rbind04.sh) rc=124 -->
| fs_bind_move21.sh | - | HANG |
| fs_bind_move22.sh | - | notrun |
| fs_bind_rbind01.sh | - | notrun |
| fs_bind_rbind02.sh | - | notrun |
| fs_bind_rbind04.sh | - | notrun |
<!-- group 70 超时(hang)->已恢复镜像 -->

<!-- group 75 (fs_bind_rbind05.sh,fs_bind_rbind06.sh,fs_bind_rbind07-2.sh,fs_bind_rbind08.sh,fs_bind_rbind09.sh) rc=124 -->
| fs_bind_rbind05.sh | - | HANG |
| fs_bind_rbind06.sh | - | notrun |
| fs_bind_rbind07-2.sh | - | notrun |
| fs_bind_rbind08.sh | - | notrun |
| fs_bind_rbind09.sh | - | notrun |
<!-- group 75 超时(hang)->已恢复镜像 -->

<!-- group 80 (fs_bind_rbind10.sh,fs_bind_rbind11.sh,fs_bind_rbind13.sh,fs_bind_rbind14.sh,fs_bind_rbind15.sh) rc=0 -->
| fs_bind_rbind10.sh | 9 | ok |
| fs_bind_rbind11.sh | 13 | ok |
| fs_bind_rbind13.sh | 9 | ok |
| fs_bind_rbind14.sh | 7 | ok |
| fs_bind_rbind15.sh | 11 | ok |

<!-- group 85 (fs_bind_rbind16.sh,fs_bind_rbind18.sh,fs_bind_rbind19.sh,fs_bind_rbind20.sh,fs_bind_rbind21.sh) rc=124 -->
| fs_bind_rbind16.sh | 7 | ok |
| fs_bind_rbind18.sh | 7 | ok |
| fs_bind_rbind19.sh | - | HANG |
| fs_bind_rbind20.sh | - | notrun |
| fs_bind_rbind21.sh | - | notrun |
<!-- group 85 超时(hang)->已恢复镜像 -->

<!-- group 90 (fs_bind_rbind23.sh,fs_bind_rbind24.sh,fs_bind_rbind25.sh,fs_bind_rbind26.sh,fs_bind_rbind28.sh) rc=124 -->
| fs_bind_rbind23.sh | - | HANG |
| fs_bind_rbind24.sh | - | notrun |
| fs_bind_rbind25.sh | - | notrun |
| fs_bind_rbind26.sh | - | notrun |
| fs_bind_rbind28.sh | - | notrun |
<!-- group 90 超时(hang)->已恢复镜像 -->

<!-- group 95 (fs_bind_rbind29.sh,fs_bind_rbind30.sh,fs_bind_rbind31.sh,fs_bind_rbind33.sh,fs_bind_rbind34.sh) rc=124 -->
| fs_bind_rbind29.sh | - | HANG |
| fs_bind_rbind30.sh | - | notrun |
| fs_bind_rbind31.sh | - | notrun |
| fs_bind_rbind33.sh | - | notrun |
| fs_bind_rbind34.sh | - | notrun |
<!-- group 95 超时(hang)->已恢复镜像 -->

<!-- group 100 (fs_bind_rbind35.sh,fs_bind_rbind36.sh,fs_bind_rbind38.sh,fs_bind_rbind39.sh,fs_bind_regression.sh) rc=124 -->
| fs_bind_rbind35.sh | - | HANG |
| fs_bind_rbind36.sh | - | notrun |
| fs_bind_rbind38.sh | - | notrun |
| fs_bind_rbind39.sh | - | notrun |
| fs_bind_regression.sh | - | notrun |
<!-- group 100 超时(hang)->已恢复镜像 -->

<!-- group 105 (fsconfig01,ftp-download-stress.sh,ftp-upload-stress01-rmt.sh,futex_cmp_requeue01,getrusage03) rc=0 -->
| fsconfig01 | 0 | ok |
| ftp-download-stress.sh | 0 | ok |
| ftp-upload-stress01-rmt.sh | 0 | ok |
| futex_cmp_requeue01 | - | notrun |
| getrusage03 | - | notrun |

<!-- group 110 (getrusage04,hangup01,ht_affinity,ht_enabled,kallsyms) rc=0 -->
| hangup01 | 0 | ok |
| ht_affinity | 0 | ok |
| ht_enabled | 0 | ok |
| kallsyms | 0 | ok |
| getrusage04 | - | notrun |

<!-- group 115 (kcmp03,kernbench,keyctl01,kill09,kill10) rc=0 -->
| kernbench | 0 | ok |
| keyctl01 | 0 | ok |
| kill09 | 0 | ok |
| kcmp03 | - | notrun |
| kill10 | - | notrun |

<!-- group 120 (kill11,leapsec01,lftest,lgetxattr01,lgetxattr02) rc=0 -->
| leapsec01 | 0 | ok |
| lftest | 0 | ok |
| lgetxattr01 | 0 | ok |
| lgetxattr02 | 0 | ok |
| kill11 | - | notrun |

<!-- group 125 (listen01,listxattr01,listxattr02,listxattr03,locktests) rc=0 -->
| listen01 | 0 | ok |
| listxattr01 | 0 | ok |
| listxattr02 | 0 | ok |
| listxattr03 | 0 | ok |
| locktests | 0 | ok |

<!-- group 130 (mallopt01,max_map_count,mbind01,memcontrol01,memfd_create01) rc=0 -->
| mallopt01 | 0 | ok |
| max_map_count | 0 | ok |
| mbind01 | 0 | ok |
| memcontrol01 | 0 | ok |
| memfd_create01 | 0 | ok |

<!-- group 135 (mmap-corruption01,mqns_01,mqns_02,mqns_03,msgrcv05) rc=0 -->
| mmap-corruption01 | 0 | ok |
| mqns_01 | 1 | ok |
| mqns_02 | 1 | ok |
| mqns_03 | 0 | ok |
| msgrcv05 | - | notrun |

<!-- group 140 (msgrcv06,msgsnd05,msgsnd06,munmap01,munmap02) rc=0 -->
| munmap01 | 0 | ok |
| munmap02 | 0 | ok |
| msgrcv06 | - | notrun |
| msgsnd05 | - | notrun |
| msgsnd06 | - | notrun |

<!-- group 145 (myfunctions.sh,net_cmdlib.sh,netns_breakns.sh,newuname01,nextafter01) rc=0 -->
| myfunctions.sh | 0 | ok |
| net_cmdlib.sh | 0 | ok |
| netns_breakns.sh | 0 | ok |
| newuname01 | 0 | ok |
| nextafter01 | 0 | ok |

<!-- group 150 (nfs01_open_files,nfs01.sh,ns-echoclient,ns-icmp_redirector,ns-icmpv4_sender) rc=0 -->
| nfs01_open_files | 0 | ok |
| nfs01.sh | 0 | ok |
| ns-echoclient | 0 | ok |
| ns-icmp_redirector | 0 | ok |
| ns-icmpv4_sender | 0 | ok |

<!-- group 155 (open_tree01,pidfd_send_signal01,pids_task1,pids_task2,ping01.sh) rc=124 -->
| open_tree01 | 0 | ok |
| pidfd_send_signal01 | 0 | ok |
| pids_task1 | 0 | ok |
| pids_task2 | - | HANG |
| ping01.sh | - | notrun |
<!-- group 155 超时(hang)->已恢复镜像 -->

<!-- group 160 (process_madvise01,process_vm01,process_vm_readv02,rename14,sched_driver) rc=0 -->
| process_madvise01 | 0 | ok |
| process_vm01 | 0 | ok |
| process_vm_readv02 | 0 | ok |
| sched_driver | 0 | ok |
| rename14 | - | notrun |

<!-- group 165 (sendfile09,shmat03,shmat1,shm_comm,shmctl01) rc=0 -->
| sendfile09 | 0 | ok |
| shmat03 | 0 | ok |
| shmat1 | 0 | ok |
| shm_comm | 1 | ok |
| shmctl01 | - | notrun |

<!-- group 170 (sigtimedwait01,sigwaitinfo01,splice05,statfs01,tbio) rc=0 -->
| splice05 | 0 | ok |
| statfs01 | 0 | ok |
| tbio | 0 | ok |
| sigtimedwait01 | - | notrun |
| sigwaitinfo01 | - | notrun |

<!-- group 175 (timens01,timerfd04,timerfd_settime02,tst_kvcmp,tst_lockdown_enabled) rc=124 -->
| timens01 | 0 | ok |
| timerfd04 | 0 | ok |
| timerfd_settime02 | - | HANG |
| tst_kvcmp | - | notrun |
| tst_lockdown_enabled | - | notrun |
<!-- group 175 超时(hang)->已恢复镜像 -->

<!-- group 180 (tst_ncpus,unzip01.sh,userfaultfd01,userns01,wait401) rc=124 -->
| tst_ncpus | 0 | ok |
| unzip01.sh | - | HANG |
| userfaultfd01 | - | notrun |
| userns01 | - | notrun |
| wait401 | - | notrun |
<!-- group 180 超时(hang)->已恢复镜像 -->

<!-- group 185 (waitid07,waitid08,waitpid07,waitpid11,wc01.sh) rc=124 -->
| wc01.sh | - | HANG |
| waitid07 | - | notrun |
| waitid08 | - | notrun |
| waitpid07 | - | notrun |
| waitpid11 | - | notrun |
<!-- group 185 超时(hang)->已恢复镜像 -->

<!-- group 190 (which01.sh,wireguard01.sh) rc=124 -->
| which01.sh | - | HANG |
| wireguard01.sh | - | notrun |
<!-- group 190 超时(hang)->已恢复镜像 -->
