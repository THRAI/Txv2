# 非 syscalls 集中的老版 API 测试（无 Summary 块，judge 计 0 分）

LTP 新版 API 测试共 1293 个(metadata 注册)。下列是各非 syscalls 集中**不在 metadata = 老版 API** 的测试,judge 无法计分。


## syscalls-ipc
semctl06 
## ipc
pipeio_1 pipeio_3 pipeio_4 pipeio_5 pipeio_6 pipeio_8 
## math
abs01 atof01 float_bessel float_exp_log float_iperb float_power float_trigo fptest01 fptest02 nextafter01 
## mm
data_space ksm01_1 ksm02_1 ksm03_1 ksm04_1 ksm06_1 ksm06_2 mallocstress01 mem02 mm01 mm02 mmap10 mmap10_1 mmap10_2 mmap10_3 mmap10_4 mmapstress02 mmapstress03 mmapstress05 mmapstress06 mmapstress07 mmapstress08 mmapstress09 mmapstress10 mtest01w mtest05 mtest06 mtest06_2 mtest06_3 overcommit_memory01 overcommit_memory02 overcommit_memory03 overcommit_memory04 overcommit_memory05 overcommit_memory06 page01 page02 shm_test01 shmt02 shmt03 shmt04 shmt05 shmt06 shmt07 shmt08 shmt09 shmt10 stack_space vma01 vma02 vma03 vma04 vma05 
## sched
hackbench01 hackbench02 pth_str01 pth_str02 pth_str03 sched_cli_serv sched_stress time-schedule01 trace_sched01 
## nptl
nptl01 
## pty
hangup01 ptem01 pty01 
## dio
dio01 dio02 dio03 dio04 dio05 dio06 dio07 dio08 dio09 dio10 dio11 dio12 dio13 dio14 dio15 dio16 dio17 dio18 dio19 dio20 dio21 dio22 dio23 dio24 dio25 dio26 dio27 dio28 dio29 dio30 
## fs
binfmt_misc01 binfmt_misc02 fs_di fs_inod01 fs_racer ftest01 ftest02 ftest03 ftest04 ftest05 ftest06 ftest07 ftest08 gf01 gf02 gf03 gf04 gf05 gf06 gf07 gf08 gf09 gf10 gf11 gf12 gf13 gf14 gf15 gf16 gf17 gf18 gf19 gf20 gf21 gf22 gf23 gf24 gf25 gf26 gf27 gf28 gf29 gf30 inode01 inode02 iogen01 isofs lftest01 linker01 openfile01 proc01 quota_remount_test01 read_all_dev read_all_proc read_all_sys rwtest01 rwtest02 rwtest03 rwtest04 rwtest05 stream01 stream02 stream03 stream04 stream05 writetest01 