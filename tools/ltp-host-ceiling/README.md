# ltp-host-ceiling — 官方计分天花板实测工具

在宿主机 Linux 上按官方 OSComp 方式（`bin/*` 无参执行 + judge 按 Summary
`passed` 累加）实测 LTP 测试的"真 Linux 天花板"。在真 Linux 上无参拿不到
passed 的文件 = 结构性 0 分，任何内核工作都无法变现；能拿分的文件才是
有效靶子。结论账本见
`msp/ltp-net-official-scoring-ledger-2026-06-10-zh.md`。

## 前置（一次性）

```sh
# 1. 与镜像同版本 LTP 原生构建（约 10 分钟）
git clone --depth 1 --branch 20240930 \
    https://github.com/linux-test-project/ltp /tmp/ltp-src
cd /tmp/ltp-src
# glibc>=2.41 需先给 include/lapi/sched.h 的 sched_attr 三定义包上
#   #if !defined(__GLIBC__) || !__GLIBC_PREREQ(2, 41) ... #endif
make autotools && ./configure --prefix=/tmp/ltp-install
make -j$(nproc) && make install
# 可选:启用 route-change-netlink(libmnl 静态本地装 + config.h 开
#   HAVE_LIBMNL 后重编该目录,见账本"route"节)

# 2. busybox 命令垫层(镜像里有而宿主缺的命令)
mkdir -p /tmp/ltp-net-sweep/shims
for c in ifconfig route arp netstat traceroute traceroute6 tftp \
         telnetd httpd ftpd udhcpd udhcpc ping6 brctl vconfig; do
  ln -sf /usr/bin/busybox /tmp/ltp-net-sweep/shims/$c
done
```

## 跑

```sh
# 单个测试
./run-one.sh <bin文件名> <输出目录>
# 全量网络子集(223 个,8 路并行,约 15 分钟)
xargs -P 8 -I{} ./run-one.sh {} results < net-sweep-list.txt
# 解析(完全复刻 judge_ltp-musl.py 口径)
python3 parse.py
```

每个测试在独立 `unshare -r -n -m` 沙箱（userns root + 私有 netns/mount）
里跑，LTP 自建 ltp_ns netns + veth 对，互不干扰、不碰宿主网络，330s 帽。

## 已知宿主伪影（解读分数时注意）

- AppArmor + userns：读 `/etc/hosts` EACCES（getaddrinfo_01 需 bind-mount
  替身）；个别 daemon 行为受限。
- 沙箱内 modprobe 永远失败：依赖未预加载模块的测试（gre/sit/can/teql 等）
  TCONF/TBROK——是环境 0 不是结构 0，需单独判断。
- 全局 sysctl（net.core.busy_read 等）在 netns 内不可见。
- busybox 版 ifconfig/route 个别输出格式与 net-tools 不同，会让少数步骤
  TFAIL（ip_tests 第 1 步 MTU）——分数按下限解读。
- net-sweep-list.txt = 官方镜像 bin/ ∩（LTP net.* / net_stress.* 清单 ∪
  testcases/network 源码树），2026-06 镜像口径。

## 真-judge 闭环（推荐口径，2026-06-10 深夜新增）

`parse.py` 是 musl judge 的复刻；**glibc judge 数的是带 ANSI 颜色的
`TPASS: \x1b[0m` 行，不能用"数 TPASS 子串"代理**（legacy-API 输出
`TPASS:\x1b[0m␣` 差一个字节，代理会错判——mc_cmds 教训）。硬真相流程：

```sh
# 彩色重扫(LTP_COLORIZE_OUTPUT=y + /etc/hosts 垫层)
xargs -P 8 -I{} ./run-one-color.sh {} results-color < net-sweep-list.txt
# 用两个原版 judge 本体逐文件打分(零自写解析层)
python3 judge-all.py   # 读 results-color/,输出 judged-real.json
```

字节级判定速查：新 C/新 shell 框架 `ESC[1;32mTPASS: ESC[0m` 两 judge 都认；
legacy-API shell（test.sh tst_resm）`ESC[1;32mTPASS:ESC[0m␣` 与 legacy C
`ESC[1;32mTPASSESC[0m␣␣:` 两 judge 都不认（且无 Summary）→ 两 lane 皆 0。

## 全量扫描（2026-06-10 深夜新增）

`run-one-full.sh`：全镜像 bin/*（2822 文件）安全跑法 —— 在网络版基础上加
pidns（`unshare -p --fork --mount-proc`，防 kill(-1)/泄漏守护进程）+
systemd 用户笼（MemoryMax=2G、TasksMax=1024，防 oom/fork 类打挂宿主）+
磁盘工作目录（/tmp 是 tmpfs，别用）。`judge-full.py` 双 judge 全量打分。
全量结果表：`msp/ltp-full-official-scoring-table-2026-06-10-zh.md`
（1028 个算分文件，musl 8594 / glibc 8574；146 个 fs 类需真 root loop
设备才能测准）。2821 文件 12 路并行约 35 分钟。
