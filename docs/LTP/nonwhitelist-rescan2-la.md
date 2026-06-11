# la 非白名单 LTP 扫描（5 个一组，组 60s 超时杀 hang，无 -I 单次执行=计分口径）

逐组追加。passed = Summary.passed（judge 口径，>0 即可加入白名单候选）。status: ok / HANG（卡住）/ notrun（qemu 早死未跑到，需复扫）。

| case | passed | status |
|---|---|---|

<!-- group 0 (creat07_child,creat09,dynamic_debug01.sh,force_erase.sh,fs_bind04.sh) rc=124 -->
| creat07_child | 0 | ok |
| creat09 | 0 | ok |
| dynamic_debug01.sh | 0 | ok |
| force_erase.sh | 0 | ok |
| fs_bind04.sh | - | HANG |
<!-- group 0 超时(hang)->已恢复镜像 -->

<!-- group 5 (fs_bind05.sh,fs_bind06.sh,fs_bind07-2.sh,fs_bind10.sh,fs_bind11.sh) rc=124 -->
| fs_bind05.sh | - | HANG |
| fs_bind06.sh | - | notrun |
| fs_bind07-2.sh | - | notrun |
| fs_bind10.sh | - | notrun |
| fs_bind11.sh | - | notrun |
<!-- group 5 超时(hang)->已恢复镜像 -->

<!-- group 10 (fs_bind13.sh,fs_bind15.sh,fs_bind16.sh,fs_bind18.sh,fs_bind19.sh) rc=124 -->
| fs_bind13.sh | - | HANG |
| fs_bind15.sh | - | notrun |
| fs_bind16.sh | - | notrun |
| fs_bind18.sh | - | notrun |
| fs_bind19.sh | - | notrun |
<!-- group 10 超时(hang)->已恢复镜像 -->

<!-- group 15 (fs_bind21.sh,fs_bind23.sh,fs_bind24.sh,fs_bind_cloneNS01.sh,fs_bind_cloneNS05.sh) rc=124 -->
| fs_bind21.sh | - | HANG |
| fs_bind23.sh | - | notrun |
| fs_bind24.sh | - | notrun |
| fs_bind_cloneNS01.sh | - | notrun |
| fs_bind_cloneNS05.sh | - | notrun |
<!-- group 15 超时(hang)->已恢复镜像 -->

<!-- group 20 (fs_bind_cloneNS06.sh,fs_bind_cloneNS07.sh,fs_bind_move03.sh,fs_bind_move06.sh,fs_bind_move07.sh) rc=124 -->
| fs_bind_cloneNS06.sh | - | HANG |
| fs_bind_cloneNS07.sh | - | notrun |
| fs_bind_move03.sh | - | notrun |
| fs_bind_move06.sh | - | notrun |
| fs_bind_move07.sh | - | notrun |
<!-- group 20 超时(hang)->已恢复镜像 -->

<!-- group 25 (fs_bind_move09.sh,fs_bind_move11.sh,fs_bind_move12.sh,fs_bind_move13.sh,fs_bind_move16.sh) rc=124 -->
| fs_bind_move09.sh | - | HANG |
| fs_bind_move11.sh | - | notrun |
| fs_bind_move12.sh | - | notrun |
| fs_bind_move13.sh | - | notrun |
| fs_bind_move16.sh | - | notrun |
<!-- group 25 超时(hang)->已恢复镜像 -->

<!-- group 30 (fs_bind_move17.sh,fs_bind_move18.sh,fs_bind_move19.sh,fs_bind_move22.sh,fs_bind_rbind01.sh) rc=124 -->
| fs_bind_move17.sh | - | HANG |
| fs_bind_move18.sh | - | notrun |
| fs_bind_move19.sh | - | notrun |
| fs_bind_move22.sh | - | notrun |
| fs_bind_rbind01.sh | - | notrun |
<!-- group 30 超时(hang)->已恢复镜像 -->

<!-- group 35 (fs_bind_rbind02.sh,fs_bind_rbind04.sh,fs_bind_rbind06.sh,fs_bind_rbind07-2.sh,fs_bind_rbind08.sh) rc=124 -->
| fs_bind_rbind02.sh | - | HANG |
| fs_bind_rbind04.sh | - | notrun |
| fs_bind_rbind06.sh | - | notrun |
| fs_bind_rbind07-2.sh | - | notrun |
| fs_bind_rbind08.sh | - | notrun |
<!-- group 35 超时(hang)->已恢复镜像 -->

<!-- group 40 (fs_bind_rbind09.sh,fs_bind_rbind11.sh,fs_bind_rbind13.sh,fs_bind_rbind14.sh,fs_bind_rbind15.sh) rc=124 -->
| fs_bind_rbind09.sh | - | HANG |
| fs_bind_rbind11.sh | - | notrun |
| fs_bind_rbind13.sh | - | notrun |
| fs_bind_rbind14.sh | - | notrun |
| fs_bind_rbind15.sh | - | notrun |
<!-- group 40 超时(hang)->已恢复镜像 -->

<!-- group 45 (fs_bind_rbind18.sh,fs_bind_rbind20.sh,fs_bind_rbind21.sh,fs_bind_rbind24.sh,fs_bind_rbind25.sh) rc=124 -->
| fs_bind_rbind18.sh | - | HANG |
| fs_bind_rbind20.sh | - | notrun |
| fs_bind_rbind21.sh | - | notrun |
| fs_bind_rbind24.sh | - | notrun |
| fs_bind_rbind25.sh | - | notrun |
<!-- group 45 超时(hang)->已恢复镜像 -->

<!-- group 50 (fs_bind_rbind28.sh,fs_bind_rbind30.sh,fs_bind_rbind31.sh,fs_bind_rbind33.sh,fs_bind_rbind36.sh) rc=124 -->
| fs_bind_rbind28.sh | - | HANG |
| fs_bind_rbind30.sh | - | notrun |
| fs_bind_rbind31.sh | - | notrun |
| fs_bind_rbind33.sh | - | notrun |
| fs_bind_rbind36.sh | - | notrun |
<!-- group 50 超时(hang)->已恢复镜像 -->

<!-- group 55 (fs_bind_rbind38.sh,fs_bind_rbind39.sh,fs_bind_regression.sh,ping01.sh,tst_kvcmp) rc=124 -->
| fs_bind_rbind38.sh | - | HANG |
| fs_bind_rbind39.sh | - | notrun |
| fs_bind_regression.sh | - | notrun |
| ping01.sh | - | notrun |
| tst_kvcmp | - | notrun |
<!-- group 55 超时(hang)->已恢复镜像 -->

<!-- group 60 (tst_lockdown_enabled,userfaultfd01,userns01,wireguard01.sh) rc=0 -->
| tst_lockdown_enabled | 0 | ok |
| userfaultfd01 | 0 | ok |
| userns01 | 0 | ok |
| wireguard01.sh | 0 | ok |
