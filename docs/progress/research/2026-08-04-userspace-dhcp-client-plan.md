# 用户态 DHCP client 接入方案

日期：2026-08-04

状态：active；QEMU 局部切片可以实现，真板验收依赖各自网卡驱动。

关联总计划：

- [`2026-08-04-portable-network-device-plan.md`](2026-08-04-portable-network-device-plan.md)
- [`2026-08-04-portable-network-before-after-explained.md`](2026-08-04-portable-network-before-after-explained.md)
- [`../plans/2026-08-04-userspace-dhcp-client.json`](../plans/2026-08-04-userspace-dhcp-client.json)

## 1. 决策和最终效果

本方案不在 txKernel 内核中实现 DHCP 协议状态机。现有 Alpine 镜像里的
BusyBox `/sbin/udhcpc` 继续负责 DHCPDISCOVER、DHCPOFFER、DHCPREQUEST、
DHCPACK、租约续期和超时重试；txKernel 只提供普通 Linux 程序应当看到的通用
网络接口。

目标链路是：

```text
BusyBox udhcpc
  │
  │ AF_PACKET/SOCK_DGRAM：发送和接收 DHCP 报文
  ▼
txKernel packet socket ABI
  │
  │ 通用 Ethernet frame TX/RX，不识别 DHCP 内容
  ▼
当前 namespace 中由 ifindex 选中的 NetDeviceOps
  │
  ├── RV64 QEMU：VirtIO MMIO NIC
  ├── LA64 QEMU：VirtIO PCI NIC
  ├── VisionFive 2：未来的 DWMAC/StarFive NIC
  └── LA 真板：未来 board profile 提供的 NIC

DHCP ACK
  │
  ▼
udhcpc default.script
  ├── rtnetlink/ioctl → 地址、链路和路由
  └── VFS write       → /etc/resolv.conf
```

这意味着以后换 QEMU 子网、换网卡顺序、换接口名或换真板时，DHCP 协议代码不变，
内核也不需要出现新的板级地址分支。

## 2. 为什么当前 `udhcpc` 不能工作

镜像中已经有真实的 BusyBox `udhcpc` 和配置脚本；问题不在于缺少命令，而在于
内核的 `AF_PACKET` 目前只是 ABI 外壳：

1. `sendto(AF_PACKET)` 校验 ifindex、复制用户数据后直接报告成功，但没有调用
   `NetDeviceOps::transmit()`。
2. 当前唯一的“回包”路径会识别 ARP request，并在同一个 socket 的内存队列里
   合成 reply；它没有经过 veth、VirtIO 或任何真实设备。
3. 设备 RX 帧直接进入 TCP/UDP/IP demux，没有复制给 packet socket。
4. `SocketTable` 不登记 packet socket，因此 RX 侧也无法枚举订阅者。
5. namespace runtime 只轮询“UP 且已经有 IPv4 地址”的设备；DHCP 恰恰要在
   地址尚未配置时接收 Offer。
6. boot network runtime 仍保有固定地址、默认路由和邻居真值；在 QEMU 默认子网
   测试时，默认租约值可能与旧静态值相同，造成 DHCP 看似成功的假阳性。

所以，仅仅在 shell 输入 `udhcpc` 或增加一个 DHCP 命令包装脚本，无法修复实际
链路。

## 3. 目标、非目标和边界

### 3.1 本计划负责

- packet socket 在所属 net namespace 中的注册、关闭撤销和隔离。
- `SOCK_RAW` 与 `SOCK_DGRAM` 的通用 Ethernet TX/RX 语义。
- 按 socket protocol 与 bound ifindex 过滤 RX，并向多个订阅者各交付一份。
- RX 入队后的 poll/ppoll/readiness 唤醒。
- 没有 IPv4 地址但链路为 UP 的接口仍可收二层帧。
- BusyBox 默认脚本需要的现有 rtnetlink/ioctl 路径回归验证。
- RV64 QEMU 的真实 SLIRP DHCP 见证；随后复用到 LA64 QEMU。
- 为 VF2 和未来 LA 真板保留同一验收 hook。

### 3.2 本计划不负责

- 在内核解析 DHCP option 或维护租约。
- DHCPv6、RA/SLAAC 或内核 DNS resolver。
- VF2 DWMAC 驱动、PHY、DMA、IRQ 实现。
- 猜测未知 LA 真板的 NIC、MMIO、IRQ、启动协议或接口名。
- 自动选择多个可能的上联网卡；多网卡场景必须由外部场景显式选择。
- 将代理、Git URL、凭据、CA 或时间策略写入网络内核。

## 4. 无硬编码契约

### 4.1 生产代码和可复用 harness 禁止出现

- `udhcpc` 进程名、DHCP UDP 端口 67/68 或 DHCP option 专用分支。
- 固定接口名，例如 `eth0`。
- 固定 IPv4/IPv6 地址、前缀、网关、DNS、代理或静态邻居。
- 固定 QEMU SLIRP 子网、服务器地址或租约地址。
- 固定 MMIO、IRQ、PCI slot、virtio bus 或板级控制器序号。
- 固定 Git remote、分支、commit、主机网卡、串口或凭据。
- 缺少输入时猜一个“常见值”继续运行。

### 4.2 允许的常量

- Linux ABI 常量：`AF_PACKET`、`SOCK_RAW`、`SOCK_DGRAM`、`PACKET_*`。
- Ethernet/ARP 标准常量：二层头长度、EtherType、`ARPHRD_ETHER`。
- 有明确失败语义的有界队列和单次轮询预算。
- 只存在于 test fixture/scenario 中的具体地址和设备放置。

测试 fixture 中的数值是输入，不允许被生产模块 import。测试还必须改变子网、
接口顺序和 ifindex，以揭露任何隐藏的环境依赖。

## 5. 必须实现的 Linux ABI 子集

### 5.1 `SOCK_RAW`

- TX：用户缓冲区已经包含完整 Ethernet header，内核逐字节交给目标 netdev。
- RX：向用户返回完整 Ethernet frame。

### 5.2 `SOCK_DGRAM`

- TX：用户缓冲区从网络层 payload 开始。内核使用目标 `sockaddr_ll` 中的目的
  MAC、目标协议和所选 netdev 的源 MAC 构造 Ethernet header。
- RX：设备收到完整 Ethernet frame；内核向用户返回去掉 Ethernet header 的
  payload。

BusyBox 1.37 的 `udhcpc` 初始收发使用这一 cooked `SOCK_DGRAM` 形式，因此不能
把它错误地当作 `SOCK_RAW` 原样透传。

### 5.3 选择和过滤

- TX 目标来自本次 `sockaddr_ll.ifindex`；未传目标时使用已经 bind 的 ifindex。
- 目标必须存在于 socket 所属 namespace、处于 UP，且是支持 Ethernet 的设备。
- RX 按 socket protocol 以及 bound ifindex 过滤；`ETH_P_ALL` 匹配所有 EtherType。
- 未绑定 ifindex 的 socket 接收该 namespace 内匹配协议的所有设备帧。
- 所有选择都使用 ifindex 到 namespace link/device projection，不依赖名字。

### 5.4 收包元数据和等待

`recvfrom/recvmsg` 的 `sockaddr_ll` 至少返回真实 ifindex、EtherType、硬件类型、
源 MAC 和 packet type。广播、多播和发给本机的帧要能区分。

当任一 socket 的队列从空变为非空时，发布 `RecvWireSet::HAS_DATA`。队列满时只
丢弃该订阅者的副本，不影响其他 packet socket，也不阻止同一帧继续进入普通
IP 栈。close 后必须先撤销 registry，再结束对象生命周期。

### 5.5 设备 busy 和错误

- 不支持的设备类型或无效二层地址：明确 errno。
- frame 超过设备 MTU 加二层头：`EMSGSIZE` 或当前统一的等价错误。
- 设备 TX busy：非阻塞 socket 返回 `EAGAIN`；阻塞 socket 通过通用等待/重试
  机制恢复，不能报告成功后丢帧。
- 设备不存在或已不属于 namespace：`ENODEV`。

## 6. 单一 ingress 所有者

每个设备帧只能调用一次 `NetDeviceOps::receive()`。正确顺序是：

```text
NetDeviceOps::receive() 一次
  │
  ├── 复制并过滤 → packet socket A
  ├── 复制并过滤 → packet socket B
  └── 原帧继续   → bridge/forwarding/IP/TCP/UDP demux
```

不能让 AF_PACKET 和 IP 栈分别读取同一个 RX 队列，否则两者会互相“抢包”。现有
boot runtime 与 namespace runtime 都可能成为 ingress owner，因此它们必须调用
同一个 packet fanout helper；后续 portable-network Phase 6 再消除 boot runtime
的私有网络真值，而不是在本计划里新增第二套收包循环。

## 7. 分阶段实施

### P0：契约、基线和安全边界

- 固化本文和 operational JSON。
- 记录当前 packet send“成功但不出设备”的 host witness。
- 明确 QEMU 默认子网会掩盖静态 boot 地址，见证必须使用外部 scenario 提供的
  非默认子网。
- DHCP/Git 测试不读取、不回显、不使用仓库文件里的明文凭据。

完成标准：评审可以逐项回答“什么在内核、什么在用户态、什么是 fixture”。

### P1：packet socket registry 和生命周期

- 在每个 `SocketTable` 增加独立的 packet socket index。
- create 成功后登记，close 时撤销。
- 保持 namespace 隔离；容量耗尽显式返回 `ENOMEM`。

完成标准：多 socket、close、不同 namespace 的 host test 通过。

### P2：通用 RX fanout

- 在设备帧进入 L3 demux 前调用统一 fanout。
- 实现 RAW/DGRAM、protocol/ifindex 过滤和 `sockaddr_ll` 元数据。
- 轮询 UP 但无 IPv4 地址的 Ethernet device。
- boot ingress 与 namespace ingress 复用同一 helper。

完成标准：veth 注入一帧，packet socket 与普通 IP consumer 都能收到各自视图；
首帧可以唤醒 ppoll。

### P3：通用 TX

- 在 net subsystem step 中解析 socket 类型和目标 link。
- RAW 原样发送；DGRAM 构造 Ethernet header。
- 调用目标 `NetDeviceOps::tx_readiness/transmit`，保留 busy/error 语义。
- syscall shim 只做用户内存和 `sockaddr_ll` ABI 翻译。
- 删除生产 synthetic ARP self-reply。

完成标准：veth peer 收到真实帧；旧的“只验证返回成功”测试被 wire 断言替代。

### P4：用户态配置控制面

- 验证 `udhcpc` 默认脚本可 flush/add 地址、link up、更新默认路由和 DNS。
- 地址与路由变化后，普通 UDP/TCP egress 必须读取当前 namespace/FIB 状态。
- 网络模式由外部配置选择：`static`、`dhcp`、`none`；生产内核无默认场景地址。

完成标准：脚本配置前后 `ip addr`、`ip route` 和实际 egress source/next-hop 一致。

### P5：RV64 QEMU 真实 DHCP

- 修复 `shell-test` 网络参数未接入 QEMU 命令的问题，或由同一 scenario renderer
  提供等价有界 runner。
- 从母盘复制临时 ext4，测试不得写坏 `local-images` 原始镜像。
- QEMU user-net 子网由 fixture 参数传入，并且不同于旧 boot 静态子网。
- guest 自动选择唯一非 loopback NIC；若有多个 NIC，场景必须显式提供 ifindex
  或接口选择器，禁止猜第一个。
- 运行真实 `/sbin/udhcpc`，然后从系统状态解析租约、默认路由和 DNS；网关从
  默认路由动态解析，不能与固定值比较。

串口见证使用稳定标记：`DHCP:BEGIN`、`DHCP:CLIENT_RC`、`DHCP:LEASE`、
`DHCP:ROUTE`、`DHCP:DNS`、`DHCP:PING`、`DHCP:END`。同时保留原生 udhcpc 的
discover/lease 输出，防止脚本伪造结果。

### P6：LA64 QEMU 与负面矩阵

- 同一 guest 脚本、ABI 和 scenario record 在 LA64 QEMU 重跑。
- 无 DHCP server、错误 ifindex、接口 down、队列满、设备 busy 必须有界失败。
- 改变子网、NIC 位置、注册顺序和 ifindex，结果不得依赖旧环境。

### P7：真板验收 hook

- VF2 在 DWMAC/StarFive 驱动完成后运行相同 guest witness。
- 未知 LA 真板只保留 target-profile/scenario 接口；型号接入前不创建虚假配置。
- 真板是否走房间 DHCP、电脑 NAT 或静态直连由外部场景决定。

## 8. 验收矩阵

| 层 | 场景 | 必须观察到 |
|---|---|---|
| registry | create/close、多 socket、多 netns | 只枚举存活且同 namespace 的订阅者 |
| TX RAW | veth、变化的 ifindex | peer 收到逐字节相同完整 frame |
| TX DGRAM | 广播/单播、变化的 MAC | 内核补正确 dst/src/EtherType header |
| RX RAW/DGRAM | 单播、广播、多播 | RAW 有 header；DGRAM 无 header；元数据正确 |
| filter | EtherType、`ETH_P_ALL`、bound/unbound ifindex | 只向匹配 socket 交付 |
| readiness | 空→非空、peek、drain、queue full | 无丢唤醒；单个满队列不影响其他 consumer |
| no-address | link UP、IPv4 为空 | 仍能收到二层帧和 DHCP Offer |
| config | 地址/路由/DNS 动态变化 | 系统状态与实际普通 socket egress 一致 |
| QEMU DHCP | 外部非默认子网 | 真 udhcpc 获得 lease，默认路由/DNS 来自结果 |
| failure | 无 server/down/busy/错误 ifindex | 有界且返回可解释错误，不假成功 |
| portability | RV64 QEMU、LA64 QEMU、VF2、LA hook | 同 ABI；差异只来自 profile/driver/scenario |

## 9. 测试与运行顺序

代码每个小阶段先运行最窄 host test，再运行：

```sh
cargo -q xtask unit
```

QEMU 前先执行对应 target 的 full build，再按 30 → 60 → 120 秒超时阶梯运行，
有进展才扩大时间。所有串口日志写入 `/tmp` 或 `msp/debug-logs` 并保留路径。

DHCP 通过后的 Git 验收分两层：

1. 可重复的本机 HTTP/HTTPS Git fixture。
2. 可选的真实 GitHub 公共仓库 shallow clone。

repository URL、代理、CA 和凭据全是显式测试输入；TLS 验证不能关闭。

## 10. Readiness 和风险

### 当前 readiness

- P1～P3 的通用 AF_PACKET + veth/host test：**已实现并通过窄测试**。
- RV64 QEMU DHCP 数据面：**已用非默认子网和真实 `udhcpc` 验证**。
- `tx.net.mode=dhcp` 下 namespace/FIB 已是地址和路由的唯一真值；兼容静态模式
  仍保留旧 QEMU 默认值，完全移出生产内核仍属于 portable-network Phase 6。
- VF2：被 DWMAC/StarFive 驱动阻塞。
- LA 真板：型号未知，只有 hook，不宣称支持。

### 主要风险

- boot 与 namespace 两条 runtime 重复读取同一设备。
- 把 `SOCK_DGRAM` 错当完整 frame。
- DHCP 前接口无地址而不被轮询。
- RX 入队与 readiness clear 之间丢唤醒。
- synthetic ARP 掩盖真实 TX 失败。
- 默认 QEMU 租约碰巧等于旧静态地址，产生假阳性。
- 配置脚本更新了 namespace，但普通 egress 仍读取 boot runtime 私有旧值。

回滚原则：任一阶段失败时保留上一阶段的 host witness；不加入协议、板级或地址
特例作为 fallback。

## 11. 与四运行方式总计划的关系

本计划是 portable-network Phase 6 的前置和输入能力。它可以先在当前 VirtIO
QEMU 设备上实现并证明 Linux packet-socket ABI，但不会把总计划 Phase 0～5、
VF2 DWMAC 或 LA 真板工作标成完成。

```text
本计划 P1-P3：通用 AF_PACKET 数据面
          │
          v
本计划 P4-P6：用户态 DHCP + 两种 QEMU 见证
          │
          v
portable-network Phase 6：删除 boot 私有网络真值，统一外部场景
          │
          ├── VF2 驱动就绪后追加真板见证
          └── LA 型号确定后填充既有 hook
```

这一拆分让当前可以先修真正阻塞 `udhcpc` 的通用能力，同时不把尚未定义好的
多设备 IRQ/DMA/真板资源契约偷偷混入 DHCP 实现。

## 12. 2026-08-04 实现结果

本轮已落地 P1～P3、RV64 DHCP 模式的 P4 子集和 P5 见证：

- packet socket 进入所属 network namespace 的注册表，close 时撤销；
- 真实设备 RX 向匹配的 RAW/DGRAM packet socket 独立 fanout，无 IPv4 地址时也轮询；
- packet `sendto` 通过 ifindex 选择真实设备，RAW 保留完整帧，DGRAM 构造二层头；
- 删除旧的“sendto 成功后在本 socket 伪造 ARP 回包”路径；
- 支持 BusyBox 查询接口所用的 `AF_INET/SOCK_RAW/IPPROTO_RAW` 控制 socket；
- `tx.runsh` 根文件系统补齐 `/sbin` overlay mountpoint，使 Alpine 自带的 `ip`、
  `udhcpc` 与默认 hook 可直接运行；
- boot delegate 不再抢读物理 RX 或维护第二个 `EtherIface`。在
  `tx.net.mode=dhcp`/`none` 下只把设备发布到 namespace，不注入地址、路由或邻居；
- `shell-test --net` 现在真实渲染进 QEMU 参数；新增参数化 RV64 DHCP/Git witness，
  母盘、内核、子网、lease、接口选择器和 Git remote 都是外部输入。多 NIC 未指定
  selector 时明确失败，不猜第一个；带凭据的 URL 被拒绝。

真实 RV64 QEMU 见证使用了与旧 `10.0.2.0/24` 不同的 fixture，观察到：

```text
udhcpc: broadcasting discover
udhcpc: lease of 172.31.44.20 obtained from 172.31.44.2
TXDHCP:gateway:172.31.44.2
TXDHCP:dns-result:pass
TXDHCP:git-result:pass
TXDHCP:git-head:f5dea58
```

真实 clone 的 remote 由 `TX_DHCP_GIT_URL` 传入；本次输入是用户指定的公开
`https://github.com/oscomp/xv6-riscv.git`。TLS 校验保持启用，CA bundle 从 guest
可读文件中按证书内容发现，未使用 `GIT_SSL_NO_VERIFY`。Git 使用 HTTP/1.1；这是
传输兼容策略而不是场景地址或仓库特例。

尚未完成：LA64 QEMU 同 witness、负面矩阵、VF2 DWMAC/StarFive 驱动、未知 LA
真板 profile，以及把兼容静态模式的旧 QEMU 地址完全迁到外部 scenario renderer。

## 13. 凭据安全旁注

只读审计发现 `local-images/git/launch-shell.sh` 含明文 GitHub PAT。本文不记录、
不读取、不使用该值。该 PAT 应在 GitHub 侧立即撤销/轮换，并把脚本改成无凭据的
参数化入口；这项凭据处置与 DHCP 内核实现独立，不能通过隐藏输出代替撤销。
