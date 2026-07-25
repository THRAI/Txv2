# IPv6 外部数据面(TCP/UDP 经网卡)现状实证与补全方案

<!-- txdoc:07-NET-IPV6-EXT-DATAPLANE-V1 -->

> 分支 `claude/ipv6-external-nic`,基线 `feature-network-refactor@2c5fe37b`。
> 这是 **Phase 0 调研正本**:先把"IPv6 外部数据面到底哪里通、哪里断"用**真机跑出来的证据**钉死,再给分阶段方案。
> 本文**未修改任何实现代码**。前序:[`IPV6_STATUS_v1.md`](IPV6_STATUS_v1.md)(V1–V3b 落地记录)、[`NET_AUDIT_v1.md`](NET_AUDIT_v1.md) §3-bis。

---

## 0. 结论速览(与任务书前提的差异)

任务书里的初步定位("外部 TCP/UDP 的设备 TX 路径疑似仍是纯 v4")**被实测推翻了一半**:

| 能力 | 实测结论 | 证据 |
|---|---|---|
| 外部 v6 **ICMPv6**(ping6) | ✅ **通** | §1.3 |
| 外部 v6 **TCP**(connect + 收发 + 关闭) | ✅ **已经通了**,无需改一行代码 | §1.4 |
| 外部 v6 TCP **off-link 经网关** | ✅ **通**(V3b 网关分支真机首验) | §1.5 |
| 外部 v6 **UDP**(含 DNS) | ❌ **全丢,一个包都到不了网线** | §1.6 |
| v6 **地址自动配置** | ❌ 无 RA/SLAAC/link-local/DHCPv6,必须手工 `ip -6 addr add` | §1.2 |
| v6 **默认路由** | ❌ 开机 `routes6` 空,必须手工 `ip -6 route add` | §1.2 |

**一句话**:v6 数据面的**流式**那半边(TCP)其实早就打通了,真正的黑洞只有 **UDP**,外加"guest 开机拿不到 v6 地址"这个使用面前提。
所以本任务的重心从"把 TCP/UDP 接到网卡"改成 **① 修 UDP 丢包真因 ② 让 v6 开机可用**。

⚠️ **环境约束(任务书 §一.3 的补充实测)**:主机确实无 v6 公网出口,验收必须打 slirp 主机侧(`fec0::2` / DNS `fec0::3`)。
另外实测到一个**会把人带沟里的 QEMU 坑**:`-netdev user,ipv6=on` **单独指定会关掉 IPv4**,必须写 `ipv4=on,ipv6=on`。详见 §1.1。

---

## 1. 现状实证(真机跑出来的,不是读代码猜的)

### 1.0 复现手法

harness 在 `.v6work/probe-v6.sh`(骨架抄自 `tools/verify-git-net.sh`),guest 脚本 `.v6work/guest-v6.sh`。要点:

- 主机起**两个独立** server(避免 dual-stack 套接字选项成为混淆变量):
  `PORT4` = `AF_INET` bind `0.0.0.0`(与 `verify-git-net.sh` 完全一致);`PORT6` = `AF_INET6` bind `::`。两者都同时提供 HTTP(TCP)+ UDP echo。
- QEMU 与 `verify-git-net.sh` 同参(块设备 `bus.0`、网卡 `bus.1`),追加
  `-netdev user,id=net,ipv4=on,ipv6=on` 与 `-object filter-dump,...,file=net.pcap`。
- guest 脚本经 `debugfs -w -R "write tx-run.sh /tx-run.sh"` 注入镜像副本,`tx.runsh=/musl/tx-run.sh` 引导;
  输出全部打 `^V6:` 前缀再 grep。镜像副本在 `trap` 里用 `: >` 截断(**不用 `rm`**)。
- **guest 侧每个网络探针必须 detach 成后台进程,stdout/stderr/rc 写三个独立文件**。两个踩过的坑:
  ① 阻塞中的 `connect()` 在本内核里**不被 busybox `timeout` 的 SIGTERM 可靠打断**,内联探针一旦 wedge 会吃掉整个 QEMU 预算;
  ② 本 FS 上 `>>` 会从 offset 0 重写(既有 append 缺陷),`cmd > f; echo rc >> f` 会把 payload 头部覆盖掉。

### 1.1 QEMU 坑:`ipv6=on` 单独指定会关掉 IPv4

第一轮用 `-netdev user,id=net,ipv6=on`,结果 **v6 全通而 v4 全死**——guest 的 SYN 一直重传、slirp 一声不吭:

```
18:05:52.319025 52:54:00:12:34:56 > 52:55:0a:00:02:02, ethertype IPv4, length 66:
    10.0.2.15.49152 > 10.0.2.2.22758: Flags [S], seq 4260165794 ...   ← 重传 20+ 次,无 SYN-ACK
```

目的 MAC(`52:55:0a:00:02:02`)正确,主机 server 也在(`curl 127.0.0.1:PORT` 通),即 **slirp 侧把 v4 关了**。
改成 `ipv4=on,ipv6=on` 后同一脚本 v4 立刻全通:

```
V6:tcp4:[rc=0;K-V4-/v4;]        ← HTTP over IPv4 OK
V6:udp4:[UDPOK-V4:PROBE4;]      ← UDP  over IPv4 OK
```

**结论**:这是**环境配置问题,不是内核问题**。后续所有 v6 harness 都必须写全 `ipv4=on,ipv6=on`,否则会伪造出"v4 回归"。

### 1.2 默认状态:guest 开机**没有任何 v6 地址、没有任何 v6 路由**

```
V6:if_inet6:[00000000000000000000000000000001 01 80 10 80       lo;]
V6:proc_route6:[]
V6:ping6_host:[3 packets transmitted, 0 packets received, 100% packet loss]
```

`/proc/net/if_inet6` 只有 `lo` 的 `::1/128`,`eth0` 一行都没有;`/proc/net/ipv6_route` 空。代码侧对齐:

- `namespace.rs:633-640`(建 `NetNamespaceDeviceLink` 的 attach 路径)把 link 的 `ipv6_addr / ipv6_prefix_len` 写死 `None`;
- boot lane 只 seed 了 v4:`crates/tx-kernel/src/init/net.rs:153-158`(`10.0.2.15/24`)+ `init/net.rs:170-186`(`0.0.0.0/0 via 10.0.2.2`),**v6 无对位**;
- `routes6` 初值空(`namespace.rs:476`),唯一写入点 `add_ipv6_route`(`namespace.rs:1192`)只有 rtnetlink 会调;
- 于是 `gateway6_for_device`(`namespace.rs:1356-1373`)恒返回 `None`,`decide_ipv6_route`(`protocol/ether/mod.rs:893`)对任何非组播 v6 目的地都落到 `Unreachable`。
- 全仓 grep `RouterAdvert|RouterSolicit|accept_ra|slaac|fe80|link_local` 在 `crates/` 下**零命中**(只有未接入的 `external/smoltcp-asterinas/` 里有)。即 **无 RS/RA、无 SLAAC、无 link-local 自动生成、无 DHCPv6**。

手工配置后一切正常:

```
$ ip -6 addr add fec0::15/64 dev eth0                       # rc=0
V6:cfg_if_inet6:[... lo; fec00000000000000000000000000015 02 40 00 80  eth0;]
V6:cfg_route6:[fec0::/64 dev eth0 scope link  src fec0::15 ;]
```

说明 rtnetlink 的 v6 地址面(V3a)是好的,`ensure_ether_iface_for_link` 的缓存键含 v6 字段(`namespace.rs:1982-1990`),改地址会重建 iface。

### 1.3 外部 ICMPv6:✅ 通(NDP 动态解析 + echo 往返)

```
V6:ping6:[rc=0; 3 packets transmitted, 3 packets received, 0% packet loss;
          round-trip min/avg/max = 10.735/60.531/159.983 ms]
```

pcap 逐帧:

```
fec0::15 > ff02::1:ff00:2 : ICMP6, neighbor solicitation, who has fec0::2
fec0::2  > fec0::15       : ICMP6, neighbor advertisement, tgt is fec0::2
fec0::15 > fec0::2        : ICMP6, echo request, id 45, seq 0
fec0::2  > fec0::15       : ICMP6, echo reply,   id 45, seq 0     (×3)
```

V2 的动态 NDP(NS 组播探测 → NA 学习)在真机上确认工作。

### 1.4 外部 IPv6 TCP:✅ **已经端到端通了**(本次调研最大的发现)

`wget -q -O - 'http://[fec0::2]:PORT6/v6'`:

```
V6:tcp6:[out=HTTPOK-V6-/round5;err=rc=0;]
host  v6| HTTP V6 /round5 from ::1        ← 主机 server 确实收到了请求
```

pcap 全握手 + 数据 + 关闭:

```
fec0::15.49152 > fec0::2.22838: Flags [S],  seq 4260165794, options [mss 65475,wscale 1,sackOK]
fec0::2.22838  > fec0::15.49152: Flags [S.], seq 64001, ack 4260165795
fec0::15.49152 > fec0::2.22838: Flags [.],  ack 1
fec0::15.49152 > fec0::2.22838: Flags [P.], seq 1:94,  ack 1, length 93     ← HTTP GET
fec0::2.22838  > fec0::15.49152: Flags [P.], seq 1:153, ack 94, length 152  ← HTTP 200
fec0::2.22838  > fec0::15.49152: Flags [F.], seq 153, ack 94
fec0::15.49152 > fec0::2.22838: Flags [.],  ack 154
```

**任务书 §一.2 的 "step_device_tx.rs grep 不到 Ipv6 分支 → 外部 TCP/UDP 疑似纯 v4" 这个推断,对 TCP 是错的**。原因是命名误导:

- `SmoltcpTcpSegment::emit_ipv4_packet()`(`protocol/tcp.rs:565`)直接用 `self.ip_repr` emit,**双族**;
  smoltcp `IpRepr::new`(`external/smoltcp-asterinas/src/wire/ip.rs:542`)在 local/remote 都是 v6 时产出 `IpRepr::Ipv6`。
- `EtherIface::dispatch_ip_at`(`protocol/ether/mod.rs:346-350`)按 IP version nibble 分派进 `dispatch_ipv6_at`(`:696`)。
- 于是 `step_device_tx.rs` **不需要** v6 分支——它是 family 无关的。

### 1.5 off-link v6(经网关):✅ 通,V3b 网关分支真机首验

```
$ ip -6 route add default via fec0::2 dev eth0      # rc=0
V6:cfg_route6:[fec0::/64 dev eth0 scope link src fec0::15; default via fec0::2 dev eth0;]
```

连一个不在 `fec0::/64` 里的地址 `2001:db8::1`,pcap 显示帧被送到**网关的 MAC**而不是目的地的:

```
52:54:00:12:34:56 > 52:56:00:00:00:02, IPv6: fec0::15.49156 > 2001:db8::1.21964: Flags [S]
52:56:00:00:00:01 > 52:54:00:12:34:56, IPv6: 2001:db8::1.21964 > fec0::15.49156: Flags [S.]
                                             ... [P.] seq 1:103 (HTTP GET 已发出) ...
```

`52:56:00:00:00:02` 正是 `fec0::2` 的 MAC ⇒ `decide_ipv6_route` 的 `Gateway` 臂 + `resolve_ndisc(gateway)` 正确工作。
(wget 最终报 `error getting response` 是因为 slirp 到不了 `2001:db8::1`,与内核无关。)

### 1.6 外部 IPv6 UDP:❌ 全丢——**真缺口在这里**

四轮不同条件的实测,v4 对照组每次都通:

| 轮次 | 条件 | v6 UDP 结果 | v4 UDP 对照 |
|---|---|---|---|
| 3/4 | ping6 与 UDP 并发 | 网线上**零包** | ✅ echo 返回 |
| 5 | 未 bind / bind 到 `fec0::15` 各一次,NDP 冷 | 网线上**零包** | ✅ |
| 8 | **先 ping6 三次热好 NDP + sleep 3s**,再未 bind/bind/重试 ×3 | 网线上**零包** | ✅ `UDPOK-V4:WARM4U` |
| 6 | 目的地址改成**组播** `ff02::1` | ✅ **发出去了**,但 **src=`::1`** | ✅ |

第 6 轮那一帧是决定性的:

```
18:21:47.392368 IP6 ::1.49156 > ff02::1.22996: UDP, length 7      ← 源地址是 ::1 !
```

第 7 轮 v6 DNS 也复现了同一个源地址缺陷(并因此拿不到应答):

```
52:54:00:12:34:56 > 52:56:00:00:00:03, IPv6: ::1.49152 > fec0::3.53: 51148+ AAAA? example.com. (29)
V6:dns6:[out=;; connection timed out; no servers could be reached;]
V6:dns4:[out=Server: 10.0.2.3; ... Address: 28.0.0.14 ...]        ← v4 DNS 正常
```

两个独立缺陷同时成立,**只修一个 UDP 仍然不通**——见 §2.1 / §2.2。

### 1.7 其它观测到的事实

- **v6 / v4 loopback TCP**:guest 内 `nc` 对 `::1:9999` 与 `127.0.0.1:9998` 都收到了数据(`LOOP6DATA` / `LOOP4DATA`),未回归。
- **v6 loopback UDP:未能测到**。busybox 的 `nc -u -l -s <addr>` 组合在本环境报
  `nc: can't connect to remote host: Address family not supported by protocol`,**v4 v6 都一样**——这是 busybox 用法限制,
  **不能**据此说 v6 loopback UDP 坏了。同时注意:`net/tests/loopback_tests/` 里 **只有 v6 TCP loopback 测试**
  (`tcp_loopback.rs:104-223`),**没有 v6 UDP loopback 测试** ⇒ [`IPV6_STATUS_v1.md`](IPV6_STATUS_v1.md) §0 表里
  "v6 loopback TCP/**UDP** ✅ 端到端通"的 UDP 那半,目前**既无真机证据也无单测证据**。
- **`ip -6 neigh` 恒空**(NDP 明明解析成功),邻居投影没接到 v6。
- **`netstat -tuna` 无数据**:`/proc/net/{tcp,tcp6,udp,udp6}` 四个文件都不存在——**v4 也一样**,不是 v6 专属缺口。
- **slirp 不会主动发 RA**:200s 抓包窗口内零 RA(libslirp 的 RA 定时器周期是几分钟级)。但 libslirp 对
  **Router Solicitation 是立即回 RA 的** ⇒ 将来做 SLAAC 时,必须**主动发 RS**,不能干等。

### 1.8 回归门基线(本分支 HEAD,未改任何代码)

| 门 | 结果 |
|---|---|
| `cargo xtask full-build --target rv64-qemu --skip-doctor` | ✅ 干净 |
| `cargo xtask full-build --target la64-qemu --skip-doctor --no-image` | ✅ `full-build: ok` |
| `cargo test -p tx-subsystems --lib -- --test-threads=1` | `832 passed; 322 failed` — **失败集合已存 `.v6work/unit-baseline.failures`(322 行)** |
| `bash tools/verify-git-net.sh` | ✅ **8 passed, 0 failed** |

---

## 2. 缺口清单(socket → step_connect → step_send → step_device_tx → 驱动)

### 2.1 G1a — UDP 设备 TX 车道是**破坏性弹出**,任何 sink 拒绝都丢数据(致命)

`crates/tx-subsystems/src/net/execution/step_device_tx.rs:342`

```rust
let Some(drain) = payload.take_udp_tx_datagram() else { return; };   // ← 先弹出,不可回退
...
match sink.transmit_at(packet.as_bytes(), now, guard) {
    PacketTxResult::Accepted { .. }          => { ... }
    PacketTxResult::Busy                     => { outcome.udp_busy += 1; }               // 数据已丢
    PacketTxResult::PendingResolution { .. } => { outcome.udp_resolution_pending += 1; } // 数据已丢
    PacketTxResult::Failed { .. }            => { outcome.udp_failed += 1; }             // 数据已丢
}
```

**另外两条车道都没有这个问题**,对比之下 UDP 是唯一会丢数据的:

- TCP(`step_device_tx.rs:261-286`):sink 拒绝时闭包 `return false`,段留在 smoltcp 队列里,下轮重发
  (这正是 commit `01aea500`「TX 弹出即丢自产洞」修 v4 大包 wedge 时确立的模式);
- raw ICMPv6(`step_device_tx.rs:396-427`):`peek_icmp6_tx_echo` → `transmit_at` → 只有 `Accepted` 才
  `commit_icmp6_tx_echo_sent`。**外部 ping6 之所以通,靠的就是这个 peek/commit,而不是别的什么 v6 特权路径。**

### 2.2 G1b — 初始 namespace 有**两个** TX sink,先跑的那个是 v4-only

一次 `net_delegate_step_once` 里的顺序(`net/delegate/runtime.rs`):

```
:247  if let Some(sink) = driver.packet_tx_sink()  → step_process_device_tx_pending_in_namespace_at(boot sink)
:280  drive_all_net_namespace_runtimes_at(...)     → 每个 configured_ether_ifaces 的 EtherPacketTxSink
```

- **boot lane 的 iface 没有任何 v6 配置**:`crates/tx-kernel/src/init/net.rs:75-84` 只有
  `IfaceCommon::with_gateway(BOOT_ETH_IPV4, BOOT_ETH_NETMASK, Some(BOOT_ETH_GATEWAY), mtu)`,
  **没有 `.with_ipv6(...)` / `.with_ipv6_gateway(...)`**,而且它是 `BootNetRuntime::new` 里一次性建的,
  netlink 改地址永远反映不到它(代码自己的注释 `init/net.rs:186-190` 已承认这个分裂)。
- 于是它对**任何非组播 v6 目的地**都是
  `decide_ipv6_route → Unreachable`(`protocol/ether/mod.rs:901`)→ `Failed{EADDRNOTAVAIL}`(`ether/mod.rs:706-714`)。
- **namespace lane 的 iface 是好的**:`ensure_ether_iface_for_link`(`namespace.rs:1994-2003`)带
  `.with_ipv6(...).with_ipv6_gateway(...)`。§1.4 的 TCP 就是它发出去的。

**G1a × G1b 相乘 = 外部 v6 UDP 必丢**:boot sink 先弹出、判 Unreachable、丢弃;namespace sink 再扫时数据已经不在了。
TCP 因为"拒绝即留队"逃过一劫,raw ICMPv6 因为 peek/commit 逃过一劫。

**证据闭合**:§1.6 第 6 轮把目的地址换成组播 `ff02::1` —— 组播在 `decide_ipv6_route` 里**不走前缀/网关判断**
(`ether/mod.rs:894-895` 直接 `Multicast`),即便是 v4-only 的 boot iface 也能解析出 `33:33:xx` MAC 并发出去。
于是**同一条 UDP 车道、同一个 socket 形态,仅仅把目的地址从单播换成组播,包就上线了**。这把 "UDP 车道本身坏了"
与 "路由判定把它判死了" 两个假设干净地分开了。

> 残留不确定性(不影响修法):第 7 轮有一个 `fec0::3` 的 DNS 查询确实上了线,说明在某些交织下
> namespace lane 会先抢到那个数据报(boot delegate 并非每轮都醒——参见既有结论"net_stress 里 boot delegate 休眠")。
> 也就是说这是一个**竞态性丢包**,只是在安静的测试里稳定表现为 100% 丢。修 G1a 之后与 boot lane 是否先跑无关。

### 2.3 G1c — v6 UDP 源地址选择是个**硬编码空洞**,上线的包 src = `::1`

`crates/tx-subsystems/src/net/structure/payload.rs:1245-1274` `udp_tx_src_hint`:

```rust
match dst.ip_addr() {
    super::IpAddress::V4(addr) => self.net_namespace().best_ipv4_route(addr)
        .and_then(|route| route.preferred_src.or_else(|| ...link 扫描兜底...))
        .map(|src| IpEndpoint::new(src, 0)),
    super::IpAddress::V6(_) => None,          // ← payload.rs:1273
}
```

hint 为 `None` 时由 smoltcp 决议源地址(`external/smoltcp-asterinas/src/socket/udp.rs:571-588`),
而这里用的 `CONTEXT_IFACE` 是一个**不带任何地址的裸 loopback iface**(`protocol/tcp.rs:701-714`);
`get_source_address_ipv6` 在无地址时回退 `Ipv6Address::LOCALHOST`(`external/.../iface/interface/ipv6.rs:75-86`)。
⇒ `drain.src = [::1]:port` ⇒ `step_device_tx.rs:345` 判 `!is_unspecified && port != 0` 成立 ⇒ 直接拿它 emit。

**§1.6 的组播帧和 §1.6 的 DNS 帧都是 `::1` 源,这条已被网线证实,不是推理。**
注意 v4 没有这个症状是因为 `get_source_address_ipv4` 在无地址时返回 `None`,smoltcp 会**静默丢弃**——
v4 完全靠 `udp_tx_src_hint` 的路由 preferred_src 活着。

### 2.4 G2 — 无地址自动配置 / 无默认 v6 路由(使用面前提)

见 §1.2。影响:**guest 开机 v6 完全不可用**,任何 v6 能力都要先手打两条 `ip -6` 命令。
v4 侧在 `init/net.rs:143-186` 有完整的"地址 + 默认路由"boot seed,v6 无对位。

### 2.5 G3 — v6-only 网卡在结构上不可能存在

`namespace.rs:1963`(`ensure_ether_iface_for_link` 第一行)`let ipv4_addr = link.ipv4_addr?;`,
`namespace.rs:1945`(`configured_ether_ifaces`)同样 `link.ipv4_addr.is_none()` 就 skip。
⇒ 没有 v4 地址的网卡根本建不出 `EtherIface`,v6 收发没有泵。双栈网卡不受影响。

### 2.6 G4 — v6 源地址选择完全不查 FIB,而且抄了三份

`best_ipv6_route`(`namespace.rs:1284`,实现完整,带 `preferred_src` / `next_hop` / 最长前缀 + oif 校验)
**全仓零个非测试调用者**。取而代之的是同一段"扫第一个 up 的非 loopback 且有 v6 地址的 link"启发式抄了三处:

| 位置 | 用途 |
|---|---|
| `crates/tx-shims/src/linux_syscall/socket/helpers.rs:100-108` | connect 时的 autobind 源地址 |
| `crates/tx-subsystems/src/net/execution/step_connect.rs:788-794` | `select_routed_local` 的 V6 臂 |
| `crates/tx-subsystems/src/net/execution/step_send.rs:813-835` | `preferred_ipv6_source_for`(**唯一调用者是 raw ICMPv6**,`step_send.rs:634`) |

后果:`ip -6 route ... src <addr>` 的 preferred_src 被忽略;多 link / 多前缀时会选错源。单前缀场景暂时看不出来。

### 2.7 G5 — 结构性 v4 偏置(不阻塞功能,记账)

- `PacketTxSink` 有 `source_ipv4()`(`net/packet/mod.rs:52`)但**没有 v6 对位**;唯一调用点是 raw-ICMPv4 车道
  (`step_device_tx.rs:437`),所以暂时不影响 TCP/UDP。
- `PacketTxResult::PendingResolution { next_hop: Ipv4Address }`(`net/packet/mod.rs:38`)是 v4 类型,
  v6 路径塞 `0.0.0.0` 哨兵(`ether/mod.rs:718-724`)。
- `emit_ipv4_packet` / `parse_ipv4_packet` 名字是纯 v4,实际双族(`protocol/tcp.rs:565/590`、`protocol/udp.rs:451/494`)
  ——**任务书的错误前提就是被这两个名字误导出来的**,值得改名或至少加醒目注释。

### 2.8 G6 — 观测面

`ip -6 neigh` 恒空;`/proc/net/{tcp,tcp6,udp,udp6}` 缺失(v4 同缺,非 v6 专属)。

### 2.9 G7 — 回归覆盖

`net/tests/external_connect_tests.rs` 里 `v6|ipv6|Inet6` **零命中**:外部 v6 TCP/UDP 全链路**没有任何单测**。
`external_udp_sendto_reaches_device_tx`(`external_connect_tests.rs:497`)只有 v4 版本。
v6 UDP loopback 也没有测试(§1.7)。

---

## 3. 方案设计(分阶段,每阶段一个可独立验证的提交)

设计原则(沿用 B 路一贯做法):**优先复用已有的双族逻辑,绝不为 v6 复制一套平行代码**。
下面每一阶段都只动"v4 已经写好、v6 忘了接上"的那一小段。

### V5-1 — 修外部 v6 UDP(核心,必须做)

同时治 G1a + G1c,两个都修才通。

**改动 1(G1a):UDP 车道弹出后可回退。** 文件 `net/execution/step_device_tx.rs`、`net/structure/payload.rs`、`net/protocol/udp.rs`。

- `udp.rs` 加 `requeue_tx_datagram(datagram: UdpTxDatagram, src: IpEndpoint)`:走已有的
  `push_datagram_inner` 形状(`udp.rs:632-646`),但 `UdpMetadata.local_address` 用**已决议的 src** 而不是
  `inner.tx_src_hint`,保证重试时 emit 出来的包与第一次逐字节一致。
- `payload.rs` 加 `restore_udp_tx_datagram(drain: SocketUdpTxDrain)` 薄封装(镜像 `take_udp_tx_datagram`,`payload.rs:922`)。
- `step_device_tx.rs::process_udp_tx_socket`:`Busy` / `PendingResolution` / `Failed` 三个臂都改成先 restore 再计数。
  ring 满时 restore 会失败——那才是真正的"best-effort 丢弃",记 `udp_failed`。

  *为什么不是"发之前先问 sink 能不能路由"*:那样只能挡住 G1b 这一种拒绝,挡不住 NDP 未解析(`PendingResolution`)
  和 TX 队列满(`Busy`)。回退是同时覆盖三种的最小充分修法,而且**顺带修掉 v4 UDP 在队列满时的静默丢包**。

- 这一步**不动 boot lane**。boot sink 仍然会对 v6 返回 `Failed`,只是不再造成数据丢失(白跑一次)。
  两个 iface 合并是结构债,记 P5(见 §4)。

**改动 2(G1c):`udp_tx_src_hint` 的 V6 臂接上真实源选择。** 文件 `net/structure/payload.rs:1273`。

- 镜像正上方的 V4 臂:`best_ipv6_route(dst)` → `preferred_src`,取不到再按 route 的 `oif_name` 扫 link 兜底。
- 这一步顺手让 `best_ipv6_route`(G4 的死代码)**第一次有真实消费者**。
- 不新增启发式:`step_send.rs:813` 的 `preferred_ipv6_source_for` 保持原样(V5-3 再收敛)。

**验收**(必须两条都过):

```
# guest: ip -6 addr add fec0::15/64 dev eth0
echo PROBE6 | nc -u -w 4 fec0::2 <PORT6>     → 期望输出 UDPOK-V6:PROBE6
nslookup example.com fec0::3                 → 期望解析成功(不再 timed out)
```

pcap 必须看到 **src = `fec0::15`(不是 `::1`)** 的 v6 UDP 包及其应答。

### V5-2 — guest 开机自动拿到可用的 v6 配置(建议做,治 G2)

**推荐方案:镜像 v4 的 boot seed,不做 RA/SLAAC。** 文件 `crates/tx-kernel/src/init/net.rs`。

在 `publish_boot_net_device_to_namespace`(`init/net.rs:143`)里,紧挨着现有的
`set_device_ipv4_addr_by_ifindex`(`:153-158`)加对位的
`set_device_ipv6_addr_by_ifindex(auth, ifindex, Some(BOOT_ETH_IPV6), Some(64))`,
`BOOT_ETH_IPV6 = fec0::15`(与 `BOOT_ETH_IPV4 = 10.0.2.15` 同款 slirp 约定)。

> **⚠️ 落地时相对本节原方案做了两处偏离,理由如下(实测驱动)。**
>
> **偏离 1:不加 `::/0` 默认路由。** 原方案说"镜像 v4 再加一条 `::/0 via fec0::2`"。
> 但 v4 的默认路由是**真的**——`10.0.2.2` 确实 NAT 到 v4 公网;而 slirp **不会**把 IPv6 路由出
> `fec0::/64`,宣告一个转发不了的默认路由等于**伪造可达性**,只会把一次快速的
> `EADDRNOTAVAIL` 变成挂到 TCP 超时。on-link 的 `fec0::2`/`fec0::3` 本来就被连接路由覆盖,
> 不需要这条。真有 v6 上游的宿主仍可 `ip -6 route add default via ...`(V3b 网关路径已真机验过,§1.5),
> 将来的 RA/SLAAC 阶段则应当**从通告里学**而不是在这里猜。
>
> **偏离 2:V5-3 从"建议做"升级为 V5-2 的必要配套,两者必须一起落。**
> 只加 V5-2 会**引入一个新回归**:eth0 一旦有了 v6 地址,connect 的 autobind
> (`helpers.rs:100`)那份 FIB-blind 启发式就会为**任何** v6 目的地(包括没有路由的全局地址)
> 选出 `fec0::15` 作为源 → SYN 入队 → `decide_ipv6_route` 判 `Unreachable` → 段被
> 拒收即留队 → **connect 挂死到超时**。实测对照:
>
> | | V5-2 单独 | V5-2 + V5-3 |
> |---|---|---|
> | `wget http://[2606:4700:4700::1111]:80/` | 55s 内未返回 | `Address not available`,**elapsed=0s** |
>
> 这恰好也是任务书 §九 那个 wget 现象的场景——**不能**让它从"快速失败"退化成"挂住"。

*为什么不做 RA/SLAAC(至少这一轮不做)*:

- 它是**独立协议栈工作量**(RS TX + RA parse + prefix→EUI-64 + 生存期管理 + link-local + DAD),按 V2 的
  NDP 规模估 250–400 LOC,而且要改 `link.ipv6_addr` 的单一主地址模型(link-local 与全局地址必须共存,
  见 §4 的 `same_ipv6_prefix` 只看主地址这条债);
- **LTP 计分价值近零**(与 [`IPV6_V4_PLAN_v1`](IPV6_V4_PLAN_v1.md) 延后 V4 的理由同源:账本里没有 RA/SLAAC 靶);
- boot seed 与现有 v4 完全同构、~40 LOC、风险接近零,能立刻让 §1.4 已经通了的 v6 TCP **开箱可用**。

*但把路留好*:§1.7 已证实 **libslirp 收到 RS 会立即回 RA**,所以将来真要做 SLAAC,验证环境是现成的
——必须主动发 RS,不能干等周期 RA。这条记进 [`IPV6_STATUS_v1.md`](IPV6_STATUS_v1.md)。

*硬编码 `fec0::15` 的取舍*:确实是 QEMU-slirp 特化,但 `BOOT_ETH_IPV4 = 10.0.2.15` 已经是同样的特化
(`init/net.rs:37`),保持一致优于引入第二种风格。**若你(用户)认为不可接受,这一阶段可以整个砍掉**,
代价是 v6 永远要手工配——V5-1 的验收脚本本来就自带 `ip -6 addr add`,不受影响。

**验收**:不打任何 `ip -6` 命令,开机直接
`cat /proc/net/if_inet6` 有 eth0 行、`ping6 fec0::2` 3/3、`wget http://[fec0::2]:PORT/` 200、`nc -u fec0::2` echo 通。

### V5-3 — v6 源地址选择收敛到 FIB(治 G4,**必做,与 V5-2 同批**)

把 §2.6 三处启发式收敛到 `NetNamespacePayload::preferred_ipv6_source(dst)`(内部走 `best_ipv6_route`,
镜像 v4 的写法),改 `helpers.rs`(connect autobind)、`step_connect.rs`(`select_routed_local`)、
`step_send.rs`(`preferred_ipv6_source_for`)、`payload.rs`(`udp_tx_src_hint`,V5-1 已先接上)。

**语义要点**:`preferred_ipv6_source` 在**没有路由覆盖 dst 时返回 `None`,这是承重的**——
connect 靠它快速失败(见 V5-2 的偏离 2)。唯一例外是 **raw ICMPv6**:它在 FIB 未命中时保留
原来的"on-link 前缀匹配 / 第一个非 `::1` 地址"启发式兜底,因为 `ping6` 打无路由地址时,
用一个尽力而为的真实本地源发包,好过 `send_raw_ipv6` 回退到 `::1`。

**验收**:见 V5-2 的对照表(无路由全局 v6 从"挂住"变回 `elapsed=0s` 快速失败);
单测集合差与基线**完全一致**。

### V5-4 — 回归覆盖补齐(治 G7,建议随 V5-1 一起提交)

`net/tests/external_connect_tests.rs` 增 v6 对位:`external_udp_sendto_reaches_device_tx` 的 v6 版
+ 一个"sink 返回 Failed 后数据报仍在队列里"的回归测试(直接钉死 G1a)。
这是唯一能在 host 单测层面挡住 G1a 复发的东西。

### 明确**不做**的(记 P5 结构债,见 §4)

- boot lane 与 namespace lane 的**双 iface 合并**(G1b 的根);
- v6-only 网卡(G3)、`PacketTxSink::source_ipv6()`、`PendingResolution` 的 v6 类型化(G5);
- v6 分片/重组、`ipv6_forwarding`、sysctl 真值(已在 [`IPV6_V4_PLAN_v1`](IPV6_V4_PLAN_v1.md) 延后);
- `/proc/net/{tcp,udp,tcp6,udp6}`、`ip -6 neigh`(G6,v4 同缺,应当作为独立的观测面任务);
- `emit_ipv4_packet` / `parse_ipv4_packet` 改名(纯重构,会碰很多调用点,单独立项更安全)。

---

## 4. 不变量与风险

| 风险 | 评估 |
|---|---|
| **动 v4 热路径?** | V5-1 改动 1 会碰 v4 UDP:`Busy`/`Failed` 从"丢弃"变成"回队重试"。这是**修正**(和 `01aea500` 给 TCP 做的一致),但必须跑满 git-net 8/8 + LTP net 抽样。改动 2 只碰 V6 臂,v4 分支一行不动。 |
| **V5-2 会不会打乱 v4?** | 只新增 v6 地址与 v6 路由,v4 的 `set_device_ipv4_addr_by_ifindex` / `add_ipv4_route` 不动。但**会改变 `ensure_ether_iface_for_link` 的缓存键**(iface 开机即带 v6),需确认 iface 重建路径没有副作用(`copy_arp_cache_from`,`ether/mod.rs:553-562`)。 |
| **netns / bridge / veth** | V5-1 改的是 socket payload 与 device TX 车道,与 netns 无关;V5-2 只 seed 初始 namespace 的 boot 设备,新建 netns 不受影响。 |
| **LTP net** | `net.ipv6` 现有 4 个 PASS(ping601/602/tracepath601/tcpdump601)靠 ICMPv6 + 控制面,V5-1/V5-2 都不碰这条路。V5-2 让 `/proc/net/if_inet6` 多一行 eth0,**理论上更像真 Linux**,但需抽样对账确认没有靶子在数行数。 |
| **不变量:v6 loopback 不回归** | 硬门槛(沿用 V1–V3b)。注意 §1.7:v6 loopback **UDP** 目前没有任何测试兜底,V5-4 应补上,否则这条不变量是空头支票。 |
| **竞态性** | G1b 的丢包是竞态(§2.2 残留不确定性),意味着**修好之前的任何 v6 UDP 测试都可能偶现通过**。判定必须看 pcap 上的源地址,不能只看"有没有回包"。 |

---

## 5. 验证计划

### 每阶段共同的回归门(硬性,不绿不进下一阶段)

1. `cargo xtask full-build --target rv64-qemu --skip-doctor --no-image` 与 `--target la64-qemu` **双架构都过**。
2. `cargo test -p tx-subsystems --lib -- --test-threads=1` 失败**集合**与基线 `.v6work/unit-baseline.failures`(322 行)
   逐行 `diff` 一致(新增测试导致的新增项要单独说明并隔离复跑)。**不看数字,看集合。**
3. `bash tools/verify-git-net.sh` = **8/8**。
4. v4 不回归:上面第 3 条已覆盖外部 v4 TCP(clone/push over 10.0.2.2);另跑 `.v6work/probe-v6.sh` 的
   `udp4` / `tcp4` 两个对照探针。
5. LTP net 抽样对账:**V5-1 落地后做一次**(重点 `net.ipv6` + `net.tcp_cmds`)。若时间不允许,**在报告里明说没做**。

### 每阶段专属验收

| 阶段 | 验收命令 | 期望 |
|---|---|---|
| V5-1 | `.v6work/probe-v6.sh`(guest 先 `ip -6 addr add fec0::15/64 dev eth0`) | `V6:udp6:[out=UDPOK-V6:...]`;pcap 有 `fec0::15.<port> > fec0::2.<port>: UDP` **且源不是 `::1`**;v6 DNS 查询以正确源上线(见下方修正) |
| V5-2 | 同上但**删掉** guest 脚本里所有 `ip -6` 命令 | `/proc/net/if_inet6` 含 eth0 行;`ping6 fec0::2` 3/3;`wget http://[fec0::2]:P/` 得 200;`nc -u fec0::2` echo 通 |
| V5-3 | 新增 host 单测 + 双前缀真机 | 源地址跟随 FIB `preferred_src` |
| V5-4 | `cargo test -p tx-subsystems --lib` | 新测试全绿;把新增测试名加进基线文件 |

> **验收标准修正(V5-1 落地时实测)**:上表 V5-1 原写的"`nslookup example.com fec0::3` 解析成功"
> **在本环境不可达**,已改判。内核侧完全正确——pcap 显示 NS/NA 解析 `fec0::3` 后,A 与 AAAA 查询
> 都以 `src=fec0::15` 正常上线;但 **slirp 回的是 ICMPv6 `destination unreachable, unreachable route fec0::3`**
> (来自 `fe80::2`)。原因是宿主 `/etc/resolv.conf` 只有 v4 nameserver(`127.0.0.53`),libslirp 没有
> v6 上游可转发。**所以 v6 DNS 的验收标准 = "查询以正确源地址上线并被 slirp 应答"**,而不是"解析成功"。

### 环境注意(全部踩过)

- QEMU netdev 必须 `ipv4=on,ipv6=on`(§1.1),否则会伪造出 v4 回归。
- 主机 server 用**两个独立套接字**(v4 一个 / v6 一个),别用 dual-stack 一个套接字省事。
- guest 探针必须 detach + 三文件(out/err/rc),别用 `>>`,别指望 `timeout` 能杀掉 wedge 的 `connect()`。
- 镜像副本用 `: >` 截断清理,**不用 `rm`**;不要并行跑两个 QEMU。
- slirp 的 `hostfwd` **只支持 IPv4**(`hostfwd=tcp:[::1]:P-[fec0::15]:P` 直接被 QEMU 拒:
  `Invalid host forwarding rule ... (Bad host address)`)⇒ **"外部主动连入 guest 的 v6 监听"在本环境无法测**,
  只能靠 guest 内 loopback + host 单测覆盖。这是环境限制,要在验收里写明,不能算内核缺陷。

---

## 6. 明确排除的假设(证伪的和证实的一样有价值)

| 假设 | 裁定 | 依据 |
|---|---|---|
| "外部 v6 TCP 不通,`step_device_tx.rs` 是纯 v4" | ❌ **证伪** | §1.4 完整 v6 HTTP 会话;`emit_ipv4_packet` 双族(`tcp.rs:565`),`dispatch_ip_at` 按版本 nibble 分派(`ether/mod.rs:346-350`) |
| "v6 UDP 丢包是 NDP 没解析(`PendingResolution`)" | ❌ **证伪** | §1.6 第 8 轮:先 ping6 三次热好 NDP + sleep 3s 再发,网线上仍然零包;重试 3 次也零包 |
| "v6 UDP 丢包是源地址选不出来导致 socket 层就拒了" | ❌ **证伪** | bind 到 `fec0::15` 的显式源同样零包(第 8 轮);且组播目的地时**同一个 socket 形态发得出去**(第 6 轮) |
| "UDP 车道本身对 v6 不工作" | ❌ **证伪** | 第 6 轮组播帧真实上线,走的就是这条车道 |
| "v4 到 10.0.2.2 有回归" | ❌ **证伪**,是环境 | `ipv4=on` 补上后立刻全通(§1.1);同分支 `verify-git-net.sh` 8/8 |
| "`best_ipv6_route` 还没实现" | ❌ **证伪** | 实现完整(`namespace.rs:1284`),问题是**零调用者**(G4) |
| "v6 loopback UDP 是通的(IPV6_STATUS_v1 §0 表)" | ⚠️ **无法证实也无法证伪** | busybox `nc -u -l -s` 在本环境 v4/v6 都不可用(§1.7);且 `loopback_tests/` 里没有 v6 UDP 测试 |
| "guest 拿不到 v6 地址是 RA 没收到" | ❌ **证伪** | 根本没有 RA/RS/SLAAC 代码(`crates/` 下零命中);而且 slirp 200s 内也不主动发 RA(§1.7) |
| "off-link v6 路由(V3b)没在真机验过" | ✅ **现在验过了** | §1.5 帧送到网关 MAC `52:56:00:00:00:02` |

---

## 7. 附:与本任务**不混在一起**的一个顺手项(RFC 6724 目的地址排序)

任务书 §九 的现象(`wget https://raw.githubusercontent.com/...` 解析到 v6 → `Address not available`)
本任务修不了(主机无 v6 出口)。但**内核侧有一个小杠杆**可以让解析器自然回退 v4:

musl/glibc 的 `getaddrinfo` 在做 RFC 6724 目的地址排序时,会对每个候选地址开一个 **UDP socket 做 `connect()` 探测**,
再用 `getsockname()` 读回源地址来判定可达性与作用域。因此:

> 若内核在"没有可用 v6 源地址 / 没有 v6 路由"时,让 UDP `connect()` 到全局 v6 地址返回
> `ENETUNREACH`(而不是成功、或返回一个不可用的源),解析器就会把 v6 候选排到最后,自动优先 v4。

- 现状线索:用户看到的 `Address not available` = `EADDRNOTAVAIL`,说明**某一步已经在失败了**,
  但排序仍把 v6 排在前面 —— 具体是 `connect()` 成功而 `getsockname()` 给了坏源,还是排序压根没跑,**尚未验证**。
- 验证方法:guest 里跑一个只调 `getaddrinfo` 的小程序打印候选顺序,同时对同一 v6 地址单独 `connect()` 看 errno。
- **裁定:不并入本任务的任何提交。** 若确认可做,单独立项、单独提交,提交信息里不得掺 v6 数据面改动。

---

## 8. 待你审阅的三个决策点

1. **V5-2 做不做、怎么做**:推荐"镜像 v4 的 boot seed(硬编码 `fec0::15/64` + `::/0 via fec0::2`)",
   明确**不做** RA/SLAAC。若你觉得往内核里再钉一个 slirp 特化地址不可接受,这一阶段可以整个砍掉。
2. **V5-1 改动 1 的范围**:回退语义会**同时改变 v4 UDP** 在 `Busy`/`Failed` 下的行为(丢 → 重试)。
   这是我认为正确的修法(与 TCP 一致),但它超出了"只修 v6"的字面范围,需要你点头。
3. **LTP net 抽样对账**放在 V5-1 之后做还是整轮结束后做(前者更早暴露问题,后者省一次长跑)。

---

## 9. 阶段状态

| 阶段 | 状态 |
|---|---|
| Phase 0 调研 | ✅ 本文;方案已审阅通过(2026-07-25) |
| V5-1 外部 v6 UDP | ✅ 落地 `1ab2b8c8`(含 V5-4 的回归测试) |
| V5-2 v6 开机自动配置 | ✅ 落地(不含 `::/0`,见偏离 1);**后续修复见 §10** |
| V5-3 v6 源选择接 FIB | ✅ 落地(与 V5-2 同批,见偏离 2) |
| V5-4 回归覆盖 | ✅ 已随 V5-1 提交 |
| V5-2 后续:v6 地址槽位 newest-wins | ✅ 落地(§10) |

**现在开箱即用的 IPv6 能力**(guest 零手工配置):`/proc/net/if_inet6` 有 eth0、
`ping6 fec0::2` 3/3、外部 v6 TCP(HTTP 200)、外部 v6 UDP(echo 往返,源地址正确)、
off-link v6 经 `ip -6 route add default via ...`(V3b)。

**仍然没有的**(有意留下,见 §3 末尾):RA/SLAAC/link-local/DHCPv6、v6-only 网卡、
v6 分片/转发、`ip -6 neigh` 投影、`/proc/net/{tcp,udp,tcp6,udp6}`、双 iface 合并。

---

## 10. V5-2 后续修复:v6 地址槽位 newest-wins(2026-07-25)

### 10.1 V5-2 引入的回归(已修)

**病症**:V5-2 的 boot seed 占住了**唯一可路由的 primary v6 槽**,于是用户/测试脚本
`ip -6 addr add <另一个地址>` 落进 `ipv6_extra_addrs`(secondary)——而 secondary 是个
**存得下但发不出的黑洞**:

- `route6_snapshot`(`namespace.rs:1138`)只从 `link.ipv6_addr`(primary)合成连接路由;
- `IfaceCommon` 只带**一个** v6 地址给 `decide_ipv6_route` 的 on-link 判断(`same_ipv6_prefix`)。

主机探针(V5-2 后、修复前):

```
primary             = fec0::15      ← boot seed 占着
best_route(peer)    = None
preferred_src(peer) = None          ← 用户配的地址完全不可用
route6_snapshot_len = 1
```

V5-2 之前 eth0 没有 v6 地址,用户的**第一个** `ip -6 addr add` 会成为 primary → 能用。
V5-2 把这条路堵了 ⇒ 真回归。

**LTP 独立印证**(§10.3 的对账):`net.ipv6` 的 `ipneigh6_ip` 用例失败形态从
"条目在表里、删除失败"退化成"条目根本没进表"——因为 LTP 的 `fd00:1:1:1::2/64` 变成
secondary 后 `decide_ipv6_route` 判 Unreachable,**NS 压根不发**,NDISC 学不到邻居。

### 10.2 修法:newest-wins + 删除时提升

- `add_device_ipv6_addr_by_ifindex`:**新地址占 primary,被顶掉的降为 secondary**。
- `del_device_ipv6_addr_by_ifindex`:对称地在删掉 primary 时**把最近降级的 secondary 提回来**,
  否则"两个地址删掉一个"会让整条链路的 v6 直接死掉。

**为什么是 newest-wins**:它在**每种情况下都不比 V5-2 之前差**——单地址行为完全相同;
多地址时"最近配置的能用"严格好过"只有第一个能用"。这仍**偏离 Linux**(Linux 路由全部地址),
真修法见 §10.4 的债。

### 10.3 v4 侧是同构的先天缺口(不在本次范围)

`add_device_ipv4_addr_by_ifindex` 是**同样的 first-wins**,`route_snapshot` 也**不看**
`ipv4_extra_addrs` ⇒ v4 secondary 同样不可路由。这解释了 `net.tcp_cmds` 里
`ipneigh01: ARP entry '10.0.0.1' not listed`(LTP 的 `10.0.0.2/24` 落成 v4 secondary),
且**基线同样失败**、与本次改动无关。

**没有顺手把 v4 也改成 newest-wins**,而且是有意的:boot seed 是 `10.0.2.15/24`,LTP 会
`ip addr add 10.0.0.2/24`;若 v4 也 newest-wins,guest 会在 LTP net 跑的过程中丢掉
`10.0.2.15` 身份 ⇒ slirp 网关 `10.0.2.2` 变成 off-prefix ⇒ **外部 v4(含 DNS)当场断**。
v4 是承重路径(git/netperf/绝大多数 LTP),不为对称性去动它。

### 10.4 记入 P5 的真修法

让 secondary 也可路由,需要两件一起做:
1. `route6_snapshot` 为 `ipv6_extra_addrs` 也合成连接路由;
2. `IfaceCommon` 携带**全部** on-link v6 前缀(小定长数组),`same_ipv6_prefix` 逐个比,
   并把它纳入 `ensure_ether_iface_for_link` 的缓存键。

做完 v4 应当同样处理(§10.3),那时 `ipneigh01_arp` / `ipneigh6_ip` 有望一起转绿。

### 10.5 验收(真机三阶段)

| 阶段 | `/proc/net/if_inet6` 的 eth0 | 外部 v6 TCP | 外部 v6 UDP |
|---|---|---|---|
| A 零配置(仅 V5-2 boot seed) | `fec0::15` | ✅ `HTTPOK-V6-/A` | ✅ `UDPOK-V6:P6A` |
| B `ip -6 addr add 2001:db8:1::15/64` | `2001:db8:1::15`(顶替,`ip -6 route` 随之切到新前缀) | 预期不可达 | 预期不可达 |
| C `ip -6 addr del ...` | **`fec0::15` 提升回来** | ✅ `HTTPOK-V6-/C` | ✅ `UDPOK-V6:P6C` |

B 阶段"不可达"是**正确行为**而非缺陷:`fec0::2` 相对新前缀是 off-link 且没有默认路由(§3 V5-2 偏离 1)。
v4 对照全程 `HTTPOK-V4-/v4`。

### 10.6 两个自己踩的方法学坑(记下来免得再犯)

1. **`cargo test` 不重建内核 ELF。** 改完源码只跑单测就去 QEMU 验证 = **验的是旧内核**;
   第一轮 C 阶段"提升没生效"正是如此(ELF 建于 13:42、源码改于 13:47),白跑两轮 QEMU。
   **每次 QEMU 验证前 `stat` 比一下 ELF 与源文件的 mtime**;`cp` 出去的提交件同理会固化旧 ELF。
2. **单测要打真实入口。** 第一版回归测试直接调 `add_device_ipv6_addr_by_ifindex`,而 guest 走的是
   rtnetlink 消息层;补了 `rtnetlink_tests.rs` 里经 `rtnetlink_handle_request` 的版本后才算真覆盖
   (两个入口都测)。
