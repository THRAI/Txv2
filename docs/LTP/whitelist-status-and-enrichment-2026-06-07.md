# LTP 白名单现状与丰富计划（2026-06-07）

## 一、白名单当前得分（最终）

| 架构 | ltp-glibc | ltp-musl | 总分 | 距 4000 |
|------|-----------|----------|------|---------|
| RV64 | **3923**/4319 | **3885**/4291 | **7808**/8610 | -77 / -115 |
| LA64 | **3938**/4306 | **3877**/4261 | **7815**/8567 | -62 / -123 |

本会话累计:rv **6950→7808(+858)**、la **6980→7815(+835)**。0 panic、无 batch hang。
(含 IPC info 修复:semctl09 SEM_INFO 用量 4/16→8/16、msgctl06 MSG_INFO 用量 2/10→4/10。SEM_STAT/MSG_STAT 数组索引语义试过会回归,未攻克。)

**对齐情况**:rv-la 已对齐(glibc la+15、musl rv+7,±15 内);glibc-musl 仍差 ~39-61(主因 waitpid01 +42,深层信号自终止,难修)。

### 阶段历史

| 阶段 | rv 总分 | la 总分 | 说明 |
|------|---------|---------|------|
| 合并修复前(基线) | 6950 | 6980 | dentry 环泄漏/ASID 修复后 |
| 修 8 个用例后 | 7143 | 7173 | open09/readahead01/mmap06/FIFO 等 |
| 丰富白名单(rv+122/la) | 7744 | 7607 | 加 ~123 得分候选 |
| round-2 + 时钟类 | 7789 | 7657 | fanotify04/08/write01/clock_settime01-02 等 |
| **la 对齐(解除 20 过时排除)** | 7789 | **7798** | la 追平 rv |
| **setgid 目录组继承修复** | **7801** | **7809** | open10/creat08 全过,4 配置各 +6 |

逐用例得分见 [`whitelist-percase-rv.md`](whitelist-percase-rv.md) / [`whitelist-percase-la.md`](whitelist-percase-la.md)。

**关键约束**：白名单是固定用例子集，满分上限就是分母（~3877–3890）。要让单项突破 4000，必须**扩大用例集**——把"能通过但不在白名单"的用例加进来（分子分母同涨）。

## 二、白名单构成（定义在哪、怎么加）

白名单不是单一列表，而是派生的（`tools/ltp-batches.py` 的 `cases_for_batch("submit")`）：

```
submit = LTP_SUBMIT_CASES（截断到 LTP_SUBMIT_ONE_POINT_FIRST_CASE 之前）
       + LTP_SUBMIT_PROMOTED_TAIL_CASES
       + LTP_BATCH_SUBMIT_NETWORK_CASES（非 LA 架构）
       − 各类排除（legacy / la / libc / glibc-network）
```

全部常量定义在 [`crates/tx-kernel/src/init/exec.rs`](../../crates/tx-kernel/src/init/exec.rs)。
**要新增用例**：把用例名追加到 `LTP_SUBMIT_CASES`（或 `LTP_SUBMIT_PROMOTED_TAIL_CASES`）。

## 三、丰富机会

- 全集可用 LTP 用例：**1411**（`build-slim-sdcard.py --list-cases ltp-musl`）。
- 当前白名单：**498**。
- **候选（可用但未入白名单）：913**。

其中相当一部分在我们内核上能直接通过，却因不在白名单而没被计分。把这些 PASS 的候选加入白名单，即可显著提分。

### 评估流程

1. `comm -23 <全集> <白名单>` 得到 913 候选。
2. 分批 `make oscomp-local-rv64 OSCOMP_LTP=<chunk>` 运行，从串口读 `FAIL LTP CASE x : 0`（0 失败 = 通过）。
3. 对 rv+la 都 PASS 的候选取交集，追加到 `LTP_SUBMIT_CASES`。
4. 重跑白名单确认提分、无回归。

## 五、进展记录与暂停状态（2026-06-07，用户要求暂停此项）

**已做的准备（保留）：**
- **cmdline 缓冲 256 → 16384 字节**（rv `boot_static.rs:CMDLINE_CAPACITY`、la `boot_facts.rs:LA64_BOOT_CMDLINE_CAPACITY`）。原本 256 字节只能塞 ~23 个用例名，导致 `OSCOMP_LTP=<长列表>` 被截断；16KB 可一次塞下全部候选。
- 候选过滤：913 → **798**（剔除特权/漏洞/压力类：bpf/fanotify/add_key/keyctl/*_module/quota/swap/dirtyc0w/dirtypipe/acct/perf_event/kexec/huge/numa/cgroup/ksm/userns/stress 等，这些在本内核上过不了还会刷屏）。重建命令：
  ```
  python3 tools/build-slim-sdcard.py --list-cases ltp-musl | sed -n 's/^  \([a-z]\)/\1/p' | sort -u > /tmp/ltp_all.txt
  python3 tools/ltp-batches.py --refresh --arch rv64 --batch submit --csv | tr ',' '\n' | sort -u > /tmp/ltp_whitelist.txt
  comm -23 /tmp/ltp_all.txt /tmp/ltp_whitelist.txt | grep -vE 'bpf|fanotify|add_key|keyctl|_module|quota|swap|dirtyc0w|dirtypipe|^acct|perf_event|kexec|huge|numa|cgroup|ksm|userns|stress' > /tmp/ltp_cand_filtered.txt
  ```

**踩的坑（重要）：**
- ❌ **不要用 `LTP_MAX_RUNTIME=5`** 做候选扫描——部分 LTP 测试会"按该时限跑满循环并狂刷输出"，刷屏且拖慢。用 LTP 默认行为即可（harness 自带 30s/例超时兜底 hang）。

**卡住现象（必须排除）：**
- ❌ **clock_settime03 在 rv+la 都挂死整个 batch**（启动后无结果、后续不再推进）。原因：`clock_settime`/`clock_adjtime`/`adjtimex`/`settimeofday`/`stime` 这类**改系统时钟**的测试会破坏 harness 基于时钟的 30s 超时 → 永久 hang。**批量扫描必须排除时钟变更类测试**（可单独跑）。
- 已跑 38 个、通过 6 个：`access03 brk01 brk02 chmod05 chown03 clock_settime01`。

**为什么慢/会卡：** 单个时钟变更类测试就能挂死整批；其余按默认超时逐个跑。策略：排除时钟类后分批扫，每批读串口 `FAIL LTP CASE x : 0` 收集通过项；若某批中途 hang，从串口取已得结果 + 定位新 hang 用例排除后续扫。

**恢复时：** 用上面命令重建 `/tmp/ltp_cand_filtered.txt` → 用自动化脚本扫 → 取 `FAIL LTP CASE x : 0` 的交集 → 追加进 `LTP_SUBMIT_CASES`。

## 六、得分项去向（重要）

- **最终目的地**：通过的候选用例名追加到 [`crates/tx-kernel/src/init/exec.rs`](../../crates/tx-kernel/src/init/exec.rs) 的 **`LTP_SUBMIT_CASES`** 常量（`case1+case2+...` 格式）。这才是真正被白名单运行+计分的列表。
- **中间收集**：自动化脚本 [`/tmp/scan_candidates.sh`](file:///tmp/scan_candidates.sh) 把每架构通过项写到 `/tmp/scan_passes_{rv,la}.txt`、卡死用例写到 `/tmp/scan_hangs_{rv,la}.txt`（易失，需及时落库）。
- **加入原则**：只加 **rv 与 la 都通过** 的候选（取交集），避免某架构分母涨而分子不涨。

### 自动化扫描脚本

`/tmp/scan_candidates.sh <rv|la> <输入清单> <每块数=30> <超时秒=150>`：分块直接跑 qemu（用已构建的 `target/oscomp/submit/kernel-{rv,la}`），每块 `timeout` 兜底；卡死的块自动跳过并把卡住用例记入 hangs 文件，通过项记入 passes 文件。无人值守。

### 判据修正（重要）

得分判据**以判分脚本 `judge_ltp-{musl,glibc}.py` 为准**：它按用例的 LTP `Summary: passed N` 计分，**得分 = passed 数**。因此收集判据是 **`passed > 0`**（有得分即收）：
- ✅ 收"有失败但有 pass"的**部分得分**用例（如 capget02 passed=5、close_range02 passed=9）；
- ❌ 排除 `passed=0` 的**无分/旧用例**（即使"干净通过"也是 0 分，如 abort01、capset02、chown04、clone02 无 Summary）；
- ❌ 排除卡死内核的用例。

（早期用过 `FAIL:0` 判据是错的——既混入无分用例又漏掉部分得分用例，已废弃。）

### 得分用例记录（rv / la 分别，judge passed>0）

权威记录见 docs/LTP 下的实时文件：
- **rv 得分用例** → [`candidate-passes-rv.txt`](candidate-passes-rv.txt)
- **la 得分用例** → [`candidate-passes-la.txt`](candidate-passes-la.txt)
- **两架构都得分(交集,实际要加入白名单的)** → [`candidate-passes-both.txt`](candidate-passes-both.txt)

下面为当前快照（**扫描进行中**，最终以上述 txt 文件为准；扫完刷新本节）：

**RV 得分用例（当前 70+，进行中）：**
```
access03 brk01 brk02 capget02 chdir04 chmod05 chown03 clone302 close_range02 creat04
epoll_pwait02 epoll_pwait03 epoll_pwait05 epoll_wait04 execl01 execle01 execlp01 execv01
execve01 execve02 execve05 execve06 execvp01 exit_group01 fchown01 fchown02 fchown03 fchown05
fcntl36 fcntl36_64 fork04 getcpu01 geteuid02 gethostname01 getpagesize01 getrandom04 getuid03
inotify_init1_01 inotify_init1_02 ioprio_get01 ioprio_set03 io_uring01 kill03 kill05 link02
llseek01 madvise02 memfd_create02 memset01 mincore02 mincore03 mkdir04 msgctl02 msgsnd01
open02 open07 pathconf02 pause01 pidfd_getfd01 pidfd_open04 pipe02 pipe07 pipe13 pipe2_02
pipe2_04 prctl02 prctl03 prctl05 prctl08
```

**LA 得分用例（当前 29+，进行中）：**
```
access03 brk01 brk02 capget02 chdir04 chmod05 chown03 clone04 clone302 close_range02 creat04
epoll_create02 epoll_pwait02 epoll_pwait03 epoll_pwait05 epoll_wait04 execl01 execle01 execlp01
execv01 execve01 execve02 execve06 execvp01 exit_group01 fchown01 fchown02 fchown03 fchown05
```

### 已知卡死内核的用例（批量扫描必须排除；本身是内核 hang bug，值得后续单独修）

```
clock_settime03  creat07
```
（clock 变更类整体排除；creat07 是 exec/ETXTBSY 路径 hang。）

## 四、本会话已修复的用例 / 改动

**内核/shim 修复:**
- 读写权限(open09/pipe03)、readahead01、mmap06、open11/mknod02/dup05（tmpfs FIFO 创建）、lseek02（FIFO→ESPIPE）、select 常规文件就绪、madvise01（建议性 advice 9..=21 no-op）。
- **setgid 目录组继承**（`tmpfs::create_inode`):父目录有 S_ISGID 时新文件继承父目录 gid 而非 caller egid → open10/creat08 由 6/9 升至 9/9（rv+la、glibc+musl 全配置各 +3）。
- 基础设施：dentry 环泄漏（LA OOM 根因）、LA ASID 扩容 63→1024（fork EAGAIN 根因）、**CMDLINE 缓冲 256→16384**。

**白名单丰富:** 加入 ~135 个得分候选到 `LTP_SUBMIT_PROMOTED_TAIL_CASES`(submit 498→626/rv)。

**rv-la 对齐:** 从 `LTP_LA_SUBMIT_EXCLUDED_CASES` 移除 20 个过时排除(它们在 la 上其实能得分:prctl02/03/05/08、semop03、sched_setparam01-05、pipe2_02/04、pipe07/13、open10、creat08、chmod05、exit_group01、readlink01、sched_setscheduler02),la 总分 +142 追平 rv。

## 五、到 4000 的路线图（每项约 -65~-126，需累积 ~10-15 个中等修复）

数据驱动(四配置逐用例失分聚合)。**绝大多数失分用例四配置一致 → 修一次 ×4 配置受益。**

| 优先 | 用例/主题 | +/配置 | 工作量 | 现状/修法 |
|------|-----------|--------|--------|-----------|
| ✅ 已修 | **setgid 目录组继承**(open10/creat08) | +6 | — | 已完成 |
| ⏳ 部分 | **SysV sem SEM_INFO 用量**(semctl09) | +4 | — | semusz/semaem 已填(4/16→8/16);剩 SEM_STAT 索引语义(+4) |
| 1 | **waitid05/06** P_PGID | +10 | 中 | waitid(P_PGID) 返 ENOSYS,需按进程组等待 |
| 2 | **statfs02+pathconf02** 路径校验 | +10 | 中 | 负向测试:空/超长/ENOTDIR/ELOOP 路径应报错(路径解析器) |
| 3 | **madvise02** | +12 | 中 | 无效上下文应拒绝 advice(负向校验) |
| 4 | **chmod01+chown05** | +14 | 中(VFS) | chmod/chown 改 fs inode 但 stat 读 RNode 缓存旧 meta;需刷新 RNode meta |
| 5 | **mq_timedsend/recv01+mq_open01** | +16 | 中 | POSIX mq 行为/errno |
| 6 | **SysV msg/shm INFO+STAT 索引** | +12 | 中 | MSG_INFO 用量 + *_STAT 按数组索引(类似 sem) |
| 7 | **timerfd01** | +8 | 中 | 定时器未触发(timerfd timer) |
| 8 | **prctl02** | +8 | 中 | 实现缺失的 prctl 选项(现 TCONF 跳过) |
| 9 | **mlock201** | +7 | 中 | VmLck 按实际页数记账(现恒 8 页) |
| 10 | open07(O_NOFOLLOW)、chown05、getpgid01、semop02、times03… | 各 ~4-6 | 中低 | 零碎 |
| 最大 | **waitpid01** | musl+31/glibc+20 | 高/风险 | 子进程 raise 信号应以 Signaled 终止,但线程继续被 exit(0) 覆盖。深层信号自终止,之前改坏过 |
| ✗ 不可修 | select03(arch 无 __NR_select)、splice07(memfd_secret) | — | — | 跳过 |

**la 特有 hang(保持排除,内核 hang bug,后续可单独修):** gettid02、mq_notify01、tgkill03/01、futex_wait03、sched_setattr01、clock_settime03、creat07、setfsgid02、tgkill02。

**结论:** 4000 可达,但是一串中等子系统修复的累积(非一招)。建议按上表 1→10 推进,优先"四配置共享 + 自包含"的(waitid/statfs/madvise/mq),避开深层(waitpid01/chmod-VFS)直到最后。
