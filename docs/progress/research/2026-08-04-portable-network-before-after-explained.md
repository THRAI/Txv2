# 网络适配方案“以前”和“现在”到底差在哪里

日期：2026-08-04

状态：解释文档；不包含驱动实现，也不表示新方案已经落地。

面向读者：第一次接触内核、驱动、MMIO、中断和 DMA 的开发者。

相关实施方案：
[`2026-08-04-portable-network-device-plan.md`](2026-08-04-portable-network-device-plan.md)

## 1. 先说最重要的结论

这里其实有三个容易混在一起的东西：

1. **仓库当前的网络代码**：主要是为 RV64 QEMU 和 LA64 QEMU 的固定 VirtIO
   网卡拓扑写的。
2. **之前的 VF2 快速方案**：在现有代码旁边直接补 JH7110/DWMAC，让当前这块
   VisionFive 2 尽快联网。
3. **现在的可移植方案**：先把“设备从哪里来、它用哪个中断、怎样做 DMA、网络
   参数从哪里来”这些公共接口理顺，再复用它支持 RV QEMU、LA QEMU、VF2，给
   未来 LA 真板留一个不造假的接入口。

因此，“现在”首先是**计划变了**，不是已经改完了很多代码。截至本文写成时，
网络驱动代码还没有因为这份新计划发生大规模修改。

之前的方案也不是完全不能工作。它有机会让**当前 VF2、当前接线、当前局域网**
尽快跑通。问题是你的最终目标后来明确成了四种模式：

- RV QEMU；
- LA QEMU；
- RV 真板，也就是现在的 VisionFive 2；
- 将来的 LA 真板。

当目标从“一块板先能用”变成“四种模式都尽量简单”，直接把 VF2 的地址、中断号、
网卡名和网络参数塞进公共代码，就会把今天的临时答案变成明天的新障碍。

现在选的是一个中间方案：**不是只修 VF2，也不是实现 Linux 那么庞大的通用设备
框架**。它只建立这四种模式真正需要的启动期静态设备路径。

## 2. 为什么 Git 明明存在，却不能 `git clone`

一次 `git clone https://...` 会实际经过下面这些软件和硬件层：

```text
Git 进程
  │  调用 libc / 系统调用
  ▼
socket、DNS、TCP、TLS
  │  产生 IP 包
  ▼
路由表、邻居表、网络接口（例如 eth0）
  │  产生以太网帧
  ▼
网卡驱动（VirtIO-net 或 DWMAC）
  │  通过 MMIO、DMA 和 IRQ 操作设备
  ▼
QEMU 虚拟网卡，或者 VF2 的 GMAC + PHY + 物理网口
```

其中任何一层没有工作，Git 最后都只会表现为解析失败、连接失败或超时。

在我们已经做过的真机检查中：

- Alpine 镜像里的 `git`、`git-remote-https` 和 CA 证书都存在；
- ext4 根盘能挂载；
- Git 本地功能可以运行；
- 但 txKernel 在 VF2 上还没有 JH7110 DWMAC/GMAC 驱动路径；
- 当前 RV64 网络初始化只认识 VirtIO，也就是 QEMU 常用的虚拟网卡；
- `/proc/net/dev` 即使出现了 `eth0`，收发计数仍然是零。

所以之前失败的主因不是 Git 命令写错，也不是再执行一次 `ip addr add` 就能解决，
而是**内核还不能驱动 VF2 的真实网卡**。

主机上执行的：

```text
sudo ip addr add ... dev enx...
sudo ip link set enx... up
```

只是在配置电脑一侧的 USB 网卡，方便 U-Boot 用 TFTP 下载内核。U-Boot 的网络驱动
不会在进入 txKernel 后自动变成 txKernel 的驱动。`StarFive #` 是 U-Boot 的命令行；
`bootm` 之后控制权交给 txKernel，两边是两个不同的软件世界。

一个很容易误判的现象是“我已经看见 `eth0` 了”。当前启动网络代码在找不到真实
注册设备时，仍可能拿到 staging/fallback 网卡记录。它证明“网络上层有一个名字”，
并不证明“真实 GMAC 已经收发数据”。判断网卡是否真正工作，至少要看到链路状态、
变化的收发计数和真实报文。

下面先不讨论怎样修改代码，而是用当前 RV64 QEMU 的一次真实 `git clone` 路径，
把每个术语放回它实际工作的地方。

### 2.1 DWMAC、GMAC、VirtIO 到底是什么关系

先区分“网络功能”和“实现这个功能的设备”。网络栈最终需要一个能够发送、接收
以太网帧的设备，但这个设备在 QEMU 和 VF2 上完全不同。

```text
                          txKernel 网络栈
                                │
                         NetDeviceOps 接口
                       ┌────────┴────────┐
                       │                 │
                VirtIO-net 驱动      DWMAC 驱动
                       │                 │
                QEMU 虚拟网卡      JH7110 GMAC 控制器
                       │                 │
                QEMU 网络后端       外部 PHY 芯片
                       │                 │
                SLIRP/TAP/bridge       网线
```

因此，你的理解有一半是对的：

- 从 txKernel 网络栈向下看，**VirtIO-net 和 DWMAC 都可以实现“网卡驱动”接口**；
- 但从硬件实现看，它们不是同一种设备，也不能使用同一个底层驱动。

具体含义如下：

| 名词 | 实际含义 | 在本项目中出现在哪里 |
|---|---|---|
| **MAC** | Ethernet Media Access Control，负责组成和解析以太网帧、MAC 地址过滤、CRC，以及与 DMA 队列交换帧数据 | 虚拟网卡或真实以太网控制器都要提供类似功能 |
| **GMAC** | 支持 Gigabit Ethernet 的 MAC 控制器 | JH7110 内部集成的千兆以太网控制器 |
| **DWMAC** | Synopsys DesignWare Ethernet MAC 控制器 IP 家族的常用名称 | VF2 的 JH7110 GMAC 驱动需要复用这类控制器的通用逻辑 |
| **stmmac** | Linux 中支持 Synopsys GMAC/DWMAC/XGMAC 的驱动框架名称 | 只作为成熟实现和硬件行为的参考，不是 txKernel 直接运行 Linux 驱动 |
| **VirtIO** | 虚拟机中的标准设备接口；它规定驱动怎样和虚拟设备协商能力、建立队列并交换缓冲区 | RV QEMU 和 LA QEMU |
| **VirtIO-net** | VirtIO 设备族中的网络设备 | QEMU 向 txKernel 提供的虚拟网卡 |

VF2 的真实链路里还会出现下面四个词：

| 名词 | 作用 |
|---|---|
| **PHY** | 完成以太网物理层工作，包括链路协商、速率、双工和电信号收发 |
| **RGMII** | JH7110 GMAC 与外部千兆 PHY 之间传输帧数据的数字接口 |
| **MDIO** | 驱动读取和配置 PHY 寄存器的管理接口，例如查询链路是否已经连接 |
| **StarFive glue** | 设置 JH7110 特有的时钟、复位、引脚、接口模式，并把这些资源交给通用 DWMAC 核心的板级代码 |

所以“实现 VF2 DWMAC”实际不是给 Git 加功能，而是补上这条路径：

```text
txKernel 网络栈
  → txKernel DWMAC 通用驱动
  → JH7110/StarFive glue
  → JH7110 GMAC
  → RGMII
  → 板载 PHY
  → RJ45 网口
```

### 2.2 当前 RV QEMU 开机时怎样出现一张网卡

以当前 RV64 QEMU user-networking 模式为例，主机侧生成的核心参数是：

```text
-netdev user,id=net0
-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.1
```

这里实际上创建了两个互相连接的对象：

```text
客体机可见的设备前端                       主机侧网络后端

virtio-net-device  ───── netdev=net0 ─────  QEMU user networking / SLIRP
        │                                         │
        │ 客体机驱动操作                           │ QEMU 代做防火墙、NAT、DNS 转发
        ▼                                         ▼
txKernel VirtIO-net 驱动                         主机真实网络
```

完整的启动路径是：

```text
xtask 生成 QEMU 命令
  │
  ├─ 创建 QEMU 网络后端 net0
  └─ 把 virtio-net-device 接到 virtio-mmio-bus.1
        │
        ▼
QEMU 生成 DTB
  │  DTB 中写入每个 virtio-mmio transport 的 MMIO 范围和 IRQ
  ▼
RV64 HAL 解析 DTB，生成 DeviceInfo / MmioRegion
  │
  ▼
KernelNetDevices 探测 MMIO 中的 VirtIO device type
  │  确认这是 network device，而不是 block device
  ▼
VirtIO-net 驱动建立 RX/TX virtqueue 和 DMA 缓冲区
  │
  ▼
内核注册 NetDeviceRegistration，当前显示名称为 eth0
  │
  ▼
启动网络代码给该接口设置 IP、路由、DNS/邻居状态
```

注意：`eth0` 是上面倒数第二步才赋予的名字。QEMU 并没有创建一个名叫 `eth0` 的
硬件；QEMU 创建的是 VirtIO 网络设备，txKernel 把已绑定的驱动实例发布成 `eth0`。

### 2.3 MMIO 是什么，它在这里具体做了什么

**MMIO** 是 Memory-Mapped I/O，中文常称“内存映射 I/O”。CPU 使用普通的加载和
存储指令访问一个地址，但该地址不是普通 RAM，而是被硬件解释成某个设备寄存器。

```text
CPU 发出 load/store
        │
        ▼
物理地址译码
   ┌────┴────────────────────┐
   │                         │
地址属于 RAM             地址属于设备 MMIO
   │                         │
读写普通内存              读写设备寄存器
                             │
                     查询状态、设置队列地址、
                     通知发送、确认中断等
```

当前 RV QEMU 文档中的一个具体实例是：

```text
virtio1@0x1000_2000
```

这里的 `0x1000_2000` 是这个 QEMU VirtIO MMIO transport 的当前物理基地址。驱动
会在这个基地址加上 VirtIO 规范规定的寄存器偏移，读取设备 ID、协商 features、
写入队列地址和发送通知。

这只是**当前 QEMU 拓扑的实例值**。驱动应该从 DTB/平台资源中获得基地址，而不是
在通用 VirtIO 驱动中认定所有机器都使用 `0x1000_2000`。

VF2 上的 DWMAC 也通过 MMIO 寄存器控制，但它有自己的物理地址和 DWMAC 寄存器
布局。VirtIO 驱动不能拿 VirtIO 寄存器偏移去操作 DWMAC，DWMAC 驱动也不能反过来
操作 VirtIO。

### 2.4 “第二个 virtio-mmio bus”是什么

VirtIO 先规定“虚拟设备怎样交换数据”，然后还需要一种方式把设备呈现给客体机。
这种呈现方式叫 **transport**。当前项目使用两种：

- RV QEMU：VirtIO over MMIO；
- LA QEMU：VirtIO over PCI。

在当前 RV QEMU 组合启动中，可以出现这样的 QEMU 内部连接：

```text
QEMU RISC-V virt machine
  │
  ├─ virtio-mmio-bus.0  ← 可能接 virtio-blk-device（根磁盘）
  │
  └─ virtio-mmio-bus.1  ← 当前明确接 virtio-net-device（网卡）
```

`virtio-mmio-bus.1` 是 QEMU 对第二个 VirtIO MMIO transport 的对象名称，编号从零
开始，所以 `.1` 表示第二个。它不是：

- VF2 上的物理总线；
- 第二根网线；
- 第二张网络接口；
- PCI slot 2。

QEMU 的这类 bus 一次只能挂一个 VirtIO 设备。如果磁盘和网卡都强行挂到
`virtio-mmio-bus.0`，QEMU 会在内核启动前就报告冲突。因此当前 xtask 把网卡明确
放到 `.1`。

QEMU 随后通过 DTB 把每个 transport 的 MMIO 地址和 IRQ 告诉客体机。内核真正
应该依赖的是 DTB 中“这一个 transport 的地址、IRQ 和实际 device type”，而不是
依赖“第二个 transport 永远是网卡”这个顺序。

### 2.5 “PCI slot 2”是什么

LA64 QEMU 没有使用 `virtio-net-device` 的 MMIO transport，而是使用：

```text
-device virtio-net-pci,netdev=net0,addr=2
```

PCI 用 Bus / Device / Function，也就是 BDF，标识总线上的功能。这里 `addr=2`
表示 QEMU 把虚拟网卡放到选定 PCI bus 的 device/slot 2；在常见的 bus 0、function
0 表示法中会对应类似 `00:02.0` 的位置。

```text
LA QEMU PCI host bridge
        │
        ▼
PCI bus 0
        │
        └─ device 2, function 0
              │
              └─ virtio-net-pci
                   ├─ Vendor/Device ID：说明它是什么设备
                   ├─ BAR：提供设备寄存器对应的 MMIO 窗口
                   └─ INTx/MSI 信息：提供中断路由
```

这里的 slot 2 是 **QEMU 客体机内部的逻辑 PCI 地址**，不是开发板上肉眼可见的
第二个插槽。

当前 LA64 代码和这个位置绑定得比较紧：QEMU 把网卡放到 slot 2，平台代码据此
计算 INTx 路由。如果以后把网卡改到 slot 3，而内核仍按 slot 2 的路由处理，就可能
出现“设备寄存器可以访问，但收包中断永远到不了正确驱动”的问题。

新方案要求 PCI 枚举结果携带实际 BDF、BAR 和中断路由。这样 slot 改变时，通用
VirtIO-net 驱动不需要修改。

### 2.6 MMIO、DMA 和 IRQ 怎样配合收发一帧

这三个概念不是三种网卡，而是驱动一张网卡时使用的三种机制。

#### 发送方向

```text
TCP/IP 生成以太网帧
        │
        ▼
驱动把帧放入 RAM 中的 TX buffer
        │
        ▼
驱动填写 RAM 中的 TX descriptor
  （记录 buffer 的 DMA 地址和长度）
        │
        ▼
驱动通过 MMIO 写设备寄存器：有新的 TX descriptor
        │
        ▼
设备通过 DMA 从 RAM 读取 descriptor 和帧
        │
        ▼
VirtIO：QEMU 取走帧
DWMAC：GMAC 把帧送到 PHY 和网线
        │
        ▼
设备完成后可以产生 IRQ，通知驱动回收 TX buffer
```

#### 接收方向

```text
QEMU 后端或物理网线收到帧
        │
        ▼
VirtIO 虚拟设备或 DWMAC 把帧 DMA 到 RX buffer
        │
        ▼
设备更新 RX descriptor
        │
        ▼
设备向中断控制器发出 IRQ
        │
        ▼
CPU 进入中断处理，获得 IRQ number
        │
        ▼
对应驱动确认设备中断并扫描 RX descriptor
        │
        ▼
网络栈处理 Ethernet → IP → TCP
        │
        ▼
唤醒等待 socket 数据的 Git 进程
```

三个术语的精确定义是：

- **MMIO**：CPU 读写设备的控制和状态寄存器；
- **DMA**：设备直接读写 RAM 中的数据缓冲区和描述符；
- **IRQ**：Interrupt Request，中断请求，设备通知 CPU 有事件需要处理。

IRQ number 是中断控制器用来区分来源的编号。例如当前 RV QEMU 网卡路径使用的
是 PLIC IRQ 2。收到编号 2 后，内核需要找到“IRQ 2 对应的那个设备驱动”，处理设备
状态，最后向设备和中断控制器完成确认。

### 2.7 `net_irq()` 是什么，为什么说它是当前限制

`net_irq()` 不是 CPU、VirtIO 或 Ethernet 标准中的术语。它是 txKernel 当前 HAL
里的一个项目自定义函数：

```text
IrqIf::net_irq() -> u32
```

它的语义大致是“返回这个平台启动网卡所用的那个 IRQ number”。当前处理关系是：

```text
平台 net_irq() 返回一个编号
        │
        ▼
内核在该编号上安装 net_rx_irq_handler
        │
        ▼
IRQ 到达，top half 保存待处理信息
        │
        ▼
task context 中的 bottom half 固定查找名为 eth0 的设备
        │
        ▼
调用该设备的 ack_interrupt_and_fire()
```

这条路径在“平台只有一张网卡，而且它一定注册为 `eth0`”时可以工作。但 IRQ 本来
属于某个具体设备实例，不属于“network”这个抽象类别。

新方案希望保存这种关系：

```text
DeviceId A ── IrqRoute(IRQ 2) ── VirtIO-net 实例 A ── 接口名 eth0

DeviceId B ── IrqRoute(IRQ 5) ── 另一网卡实例 B  ── 接口名 eth1
```

这样即使只有一张网卡，也能保证 IRQ、驱动实例和接口投影来自同一次绑定，不需要
先拿到 IRQ 再按字符串 `eth0` 猜设备。

### 2.8 `eth0` 是什么，我们是不是要增加多张网卡

`eth0` 是网络接口名称。用户态通过它查看统计、设置地址和选择路由，但这个字符串
不是硬件身份。

```text
硬件或虚拟设备
        │
        ▼
驱动实例
        │
        ▼
内核中的稳定设备身份 DeviceId
        │
        ▼
网络命名空间中的接口投影
        │
        ├─ 名称：eth0
        ├─ ifindex：接口编号
        ├─ IP 地址
        └─ RX/TX 统计
```

当前计划**不是要求你给 VF2 安装多张网卡**。正常情况下每个目标完全可以只有一张
工作网卡。公共代码覆盖零张、一张和多张，是为了保证：

- 没有网卡时明确报告无设备，不生成假的 `eth0`；
- 有一张网卡时使用它自己的资源；
- 测试加入第二个设备或无关设备时，不会因为“取第一个”而选错；
- 以后真的出现第二张网卡时，不必重写 IRQ 和注册模型。

当前 `eth0` 的问题不是这个名字本身不能使用，而是 IRQ 下半部把它当成了定位硬件
的唯一依据。另外，当前 staging/fallback 也可能发布这个名字，所以“存在 `eth0`”
不等于“DWMAC 或 VirtIO 驱动初始化成功”。

### 2.9 IP、路由、邻居表和 QEMU SLIRP 分别是什么

这些属于网络配置和网络协议，不属于网卡寄存器。

| 名词 | 精确作用 |
|---|---|
| **接口 IP 地址** | 标识本机这个网络接口在 IP 网络中的地址，例如当前 QEMU guest 的 `10.0.2.15` |
| **prefix/netmask** | 说明哪些目标 IP 与本机在同一子网，例如 `/24` 对应 `255.255.255.0` |
| **路由表** | 根据目标 IP 选择输出接口和下一跳；非本地目标通常交给默认网关 |
| **网关** | 帮本机把 IP 包转发到其他网络的下一跳路由器 |
| **邻居表** | 保存同一 Ethernet 链路上“下一跳 IP → MAC 地址”的对应关系；IPv4 通常通过 ARP 学习 |
| **DNS** | 把 `github.com` 这样的域名解析为 IP 地址 |
| **SLIRP/user networking** | QEMU 在主机用户态实现的虚拟网络后端，提供虚拟 DHCP、网关、DNS 转发、防火墙和 NAT，不要求创建 TAP |

上一版写的“QEMU SLIRP 的邻居地址”不够准确。准确说法应当是：当前启动代码固定了
**QEMU user networking 的接口 IP、默认路由、DNS 相关地址和静态邻居表项**。

QEMU 官方 user networking 的默认拓扑是：

```text
                         主机与 Internet
                               ▲
                               │ NAT / 转发
                               │
                    QEMU SLIRP 网关 10.0.2.2
                               ▲
                               │
        ┌──────────────────────┴──────────────────────┐
        │             虚拟 Ethernet 子网              │
        │                  10.0.2.0/24                 │
        │                                              │
guest 接口 10.0.2.15                         DNS 10.0.2.3
```

这里有两种完全不同的“地址查找”：

```text
DNS：github.com  ──解析──> GitHub 服务器的 IP

邻居表：下一跳 IP 10.0.2.2 ──ARP/静态表──> 下一跳的 Ethernet MAC 地址
```

一次访问 GitHub 时会按下面顺序发生：

1. Git 请求解析 `github.com`；
2. DNS 查询发往 `10.0.2.3`；
3. DNS 返回 GitHub 的公网 IP；
4. 路由表发现这个公网 IP 不在 `10.0.2.0/24`，选择默认网关 `10.0.2.2`；
5. 邻居表给出 `10.0.2.2` 对应的目标 MAC；
6. txKernel 把 IP 包封装成发往该 MAC 的 Ethernet frame；
7. VirtIO-net 把 frame 交给 QEMU SLIRP；
8. SLIRP 在主机侧做 NAT，再通过主机网络访问 GitHub。

到了 VF2 真板，这一段会变成：

```text
VF2 的实际 IP/前缀
        │
        ├─ 实际局域网的默认网关
        ├─ 实际可用的 DNS
        └─ DWMAC → PHY → 网线 → 交换机/路由器
```

VF2 不存在 QEMU SLIRP。你之前给主机 USB 网卡设置的 `192.168.1.100/24`，只是当前
主机与 U-Boot TFTP 链路的一项配置。txKernel 启动后是否沿用同一子网、谁做网关、
怎样访问互联网，必须由实际接线和主机是否配置转发/NAT 决定，不能从
`192.168.1.100` 自动推导出来。

### 2.10 把所有概念放进一次真实 `git clone`

下面按发生时间把链路串起来。例子仍是 RV64 QEMU user networking：

```text
【开机阶段】

1. xtask 启动 QEMU
      ├─ virtio-net-device：客体机看见的虚拟网卡
      ├─ virtio-mmio-bus.1：这个网卡在 RV QEMU 中使用的 transport
      └─ SLIRP net0：QEMU 主机侧网络后端

2. QEMU 生成 DTB
      └─ 告诉 txKernel：VirtIO MMIO 地址、范围和 IRQ number

3. txKernel 初始化设备
      ├─ MMIO：读取设备 ID、协商能力、设置队列
      ├─ DMA：建立 RX/TX buffer 与 descriptor ring
      ├─ IRQ：安装该设备完成/收包时使用的处理函数
      └─ eth0：把已初始化设备发布给网络栈和用户态的接口名

4. txKernel 应用网络配置
      ├─ IP：10.0.2.15/24
      ├─ default gateway：10.0.2.2
      ├─ DNS：10.0.2.3
      └─ neighbor：下一跳 IP 到 MAC 的映射

【运行 git clone】

5. Git/libc 发出 DNS 查询
      └─ UDP/IP/Ethernet → eth0 → VirtIO TX → SLIRP → DNS

6. Git 创建到 GitHub IP:443 的 TCP 连接
      └─ 路由选择 10.0.2.2 → 邻居表选目标 MAC → VirtIO TX → SLIRP NAT

7. Git 与 GitHub 完成 TLS
      └─ 校验证书、域名和当前时间

8. Git 发送 HTTP 请求并接收仓库数据

【接收每批数据】

9. SLIRP 把返回 frame 放入 VirtIO 设备
      └─ DMA 写入 txKernel 预先准备的 RX buffer

10. VirtIO 设备触发 IRQ
      └─ CPU 进入中断 → 驱动确认中断 → 扫描 RX descriptor

11. 网络栈处理 Ethernet/IP/TCP 数据
      └─ 数据进入 Git 的 socket，Git 写入 ext4 文件系统
```

VF2 当前缺的是第 3 步中面向 DWMAC 的设备初始化、DMA ring、PHY 和 IRQ 路径。
第 4 步也需要换成真实网络配置。Git、TLS、socket 和 ext4 不是当前主要缺口。

## 3. 旧做法在做什么

旧的 VF2 快速思路大致是：

1. 找出这块 VF2 上某个 DWMAC 控制器当前使用的 MMIO 地址、中断号、PHY 地址；
2. 写 DWMAC 驱动和 StarFive 专用初始化；
3. 把它注册成固定名字 `eth0`；
4. 让全局的 `net_irq()` 指向这个控制器的中断；
5. 在启动代码里放入当前局域网的 IP、网关和 DNS；
6. 执行一次固定仓库的 GitHub clone 作为验收。

这条路径可以在一个已知环境中工作，但驱动选择、设备资源、接口名称和网络参数之间
没有明确的数据关联。设备顺序、板型或网络拓扑变化后，公共代码仍会继续使用原值，
而不是从当前机器重新获得事实。

旧做法的问题不是“代码里绝对不能出现数字”，而是把**当前运行环境的事实**误当成
了**所有机器都遵守的规则**。

## 4. 当前代码里有哪些“只在熟悉环境中碰巧成立”的假设

| 当前假设 | 为什么现在能工作 | 换一种情况会怎样 |
|---|---|---|
| 看到 RISC-V 架构就走 RV64 QEMU VirtIO 路径 | RV64 QEMU 确实用 VirtIO | VF2 也是 RISC-V，但它的板载网卡是 DWMAC，走错驱动 |
| 一个设备只有一段 MMIO 和一个 IRQ | 简单 VirtIO 测试拓扑够用 | GMAC 可能还关联时钟、复位、MDIO、PHY 等资源，信息表达不完整 |
| 整个平台只有一个 `net_irq()` | 当前只考虑一个启动网卡 | 两张网卡或网卡位置变化时，无法可靠知道中断属于谁 |
| 网络中断下半部固定查 `eth0` | 当前第一张网卡恰好叫这个名字 | 改名、换默认网卡或多网卡后，可能唤醒错误设备 |
| RV QEMU 网卡固定在第二个 virtio-mmio bus | 当前 xtask 就这样启动 QEMU | 改设备顺序、插入另一个 VirtIO 设备后，位置假设失效 |
| LA QEMU 网卡固定在 PCI slot 2 | 当前 QEMU 命令把它放在 slot 2 | slot 改变后，中断路由和设备选择可能不再对应 |
| 启动网络固定为 `10.0.2.x` 和 QEMU SLIRP 的邻居地址 | QEMU user networking 使用这套约定 | 真板局域网、TAP、bridge 或另一台主机都不是这个网络 |
| 找不到真实网卡时仍给上层一个 fallback | 方便早期开发网络栈 | 用户看到 `eth0`，却误以为硬件驱动已经工作 |

这些假设单独看都不大，但它们分散在 HAL、设备初始化、中断、网络启动和 xtask 中。
这就是为什么最后的症状只有一句“Git clone 不通”，根因却不在 Git 那一个文件里。

## 5. 新做法在做什么

新方案把“当前机器的事实”作为数据交给公共代码，而不是让公共代码猜。

- **固件描述、DTB 或 PCI 枚举**提供当前机器实际存在的设备和资源；
- **DeviceId** 是内核分配给设备实例的稳定标识，接口改叫 `eth0` 或 `eth1` 时仍可
  关联同一个设备；
- **资源记录**说明这个设备有哪些 MMIO、IRQ、时钟、复位、PHY 和 DMA 条件；
- **驱动匹配**根据 DT compatible、PCI ID 或 VirtIO device type 选择驱动；
- **IrqRoute** 记录设备、中断控制器和 IRQ number 之间的关联；
- **DmaDomain/DmaConstraints** 记录设备可访问的 DMA 地址范围、对齐和缓存一致性要求；
- **NetBootConfig** 提供要使用的接口、IP、路由、DNS 等启动网络配置；
- **xtask target profile** 在主机侧选择镜像、启动方式和测试场景。

核心路径可以简化成：

```text
板级/固件事实
    ↓
不可变的设备资源表
    ↓
按 compatible 或总线身份匹配静态驱动
    ↓
生成“DeviceId + 该设备的 IRQ route + 该设备的 DMA 条件”
    ↓
发布一个或多个网卡
    ↓
外部配置选择使用哪张卡，并设置地址、路由、DNS
```

这里仍然采用 txKernel 已经选择的**静态平台**方式。不同目标仍在编译/链接时选择具体
的 `TxPlatform` 和驱动集合，不引入运行时 HAL manager，也不做 USB 热插拔那一套
大型动态设备模型。

## 6. “以前”和“现在”的逐项对比

| 方面 | 以前的快速方案/当前旧路径 | 现在的计划 |
|---|---|---|
| 主要目标 | 让已知 VF2 或既定 QEMU 拓扑先工作 | 同一公共路径服务三种已知模式，并给未来 LA 真板留接口 |
| 设备从哪里来 | 按架构分支、名字、顺序或熟悉地址选择 | 平台发布资源事实，公共 binder 按设备身份和能力匹配 |
| 板卡地址 | 容易直接写进初始化或驱动 | 来自该目标的固件/板级资源提供者 |
| 驱动选择 | “RISC-V 就试 VirtIO”或“第一个成功的设备” | 根据 compatible、PCI ID 或总线能力选择 |
| 设备身份 | 主要依靠 `eth0` 这样的显示名字 | 用稳定 `DeviceId` 关联资源、中断和设备；名字只是用户界面 |
| 中断 | 全平台一个网络 IRQ | 每个设备携带自己的 IRQ route |
| DMA | 默认整个平台都一样 | 每个设备带地址范围、对齐和一致性等约束 |
| 多网卡 | 没有完整语义 | 可以注册零张、一张或多张，再由配置选默认设备 |
| 没有网卡 | 可能出现 staging/fallback `eth0` | 明确报告没有可用设备，不伪装成功 |
| IP/网关/DNS | 启动代码内置 QEMU SLIRP 数值 | 启动参数、场景、DHCP 或用户态配置提供 |
| RV QEMU | 依赖固定 VirtIO 位置 | 从固件资源发现每个 transport，并绑定实际 IRQ |
| LA QEMU | 依赖固定 PCI slot 和区域名 | 枚举实际 PCI 身份、BAR 和中断路由 |
| VF2 | 没有 DWMAC 驱动 | 公共 DWMAC 核心加 StarFive 板级 glue |
| LA 真板 | 现在不知道型号，无法诚实实现 | 只冻结静态接入口，不编造 MMIO、IRQ 或驱动 |
| Git 验收 | 容易固定仓库和当前网络 | 仓库、分支、网络场景都是测试输入，HTTPS 证书必须验证 |

## 7. 为什么看起来会改很多地方

因为一块网卡不是只和“网络驱动文件”打交道。它从开机到收包会穿过多个所有者：

```text
板级信息 → HAL → 设备绑定 → DMA/中断 → 网络接口 → IP 配置 → xtask 验收
```

现在每一层都藏了一小部分“当前 QEMU 就是这样”的知识。要让它们以后不互相矛盾，
需要在每个知识真正所属的位置各改一小块，而不是把所有判断堆进 DWMAC 驱动。

预计涉及的范围如下：

| 范围 | 要做什么 | 是否重写 |
|---|---|---|
| 设计文档 | 先明确资源、绑定、IRQ、DMA 和配置由谁负责 | 不是代码重写 |
| `tx-hal` 数据类型 | 把单 MMIO/单 IRQ 扩成能表达设备完整资源的记录 | 修改公共接口 |
| board/platform crate | 从该机器的 DTB、PCI 或静态板级事实产生资源表 | 每个平台各自的小适配 |
| `tx-kernel::devices` | 去掉按 CPU 架构猜网卡，改为公共匹配和绑定 | 重构启动设备路径 |
| 中断代码 | 从全局 `net_irq()` 和固定 `eth0` 改成设备关联 route | 局部但重要的重构 |
| DMA 辅助层 | 让分配和同步遵守具体设备约束 | 扩展现有抽象 |
| 启动网络配置 | 移出 QEMU 固定 IP、邻居和默认网卡 | 改配置来源，不重写 TCP/IP |
| DWMAC/StarFive | 新增 VF2 真正缺失的控制器和 SoC glue | 新驱动，是 VF2 最大的新代码块 |
| xtask 和测试 | 为目标、设备排列和网络场景生成参数并做回归 | 主机工具与验证改动 |

所以它确实会**触及多个模块**，但这不等于重写整个内核。跨层修改的原因是把已经
分散的责任接正确，而不是扩大项目范围。

## 8. 哪些东西不会改

这次不需要因为设备可移植性而重写：

- Alpine Git 镜像和 Git 程序；
- ext4/VFS 的基本挂载路径；
- 进程、线程和普通文件描述符模型；
- 已经能工作的 socket/TCP/IP 主体语义；
- 调度器、虚拟内存和页表总体架构；
- txKernel 的编译期静态 `TxPlatform` 选择；
- U-Boot 的 TFTP 下载功能。

这些上层或旁支模块仍然要参加最终回归测试，但不是本次设计要推倒重来的对象。

另外，现在明确**不做**以下事情：

- 不实现 Linux 完整的动态 driver model；
- 不支持任意未知板子插上就自动识别；
- 不做 USB/PCIe 热插拔设备生命周期；
- 不要求一个内核二进制同时跑所有架构和板子；
- 不凭空给未知 LA 真板填写假的型号、网卡、MMIO 或 IRQ；
- 不为了“看起来能启动”保留会伪装成功的网络 fallback。

## 9. “不能硬编码”不等于“代码里不能有常量”

这点很重要。网卡驱动必然有寄存器偏移、状态位和协议编号。如果连这些都不能写，
驱动就无法存在。

| 可以放在代码里的常量 | 不该放在公共代码里的环境事实 |
|---|---|
| DWMAC 手册规定的寄存器偏移 | 这块板上 DWMAC 的物理基地址 |
| 描述符格式和 bit mask | 当前网卡的 IRQ 号码 |
| Ethernet EtherType | 当前局域网 IP、网关、DNS |
| PCI vendor/device ID | 网卡一定在 PCI slot 2 |
| DT compatible 字符串 | “第一个设备一定是网卡” |
| 队列容量上限 | 固定 GitHub 仓库、主机网卡名或串口名 |

一句话：**硬件/协议规格规定的数字属于驱动；某一次部署观察到的数字属于平台数据或
测试场景。**

## 10. 为什么不直接先写 VF2 驱动

如果现在直接写 DWMAC 驱动，驱动很快会遇到几个没有公共答案的问题：

- 它从哪里获得不止一段的硬件资源？
- 它的 IRQ 如何与这一张网卡绑定，而不是使用全局 `net_irq()`？
- DMA 缓冲区能放在哪些地址，缓存需要怎样同步？
- 多张网卡时，中断下半部怎样找到正确设备？
- IP 和默认路由由谁设置？
- 找不到硬件时，应该报错还是偷偷创建一个假 `eth0`？

如果驱动自己临时回答这些问题，StarFive 专用代码就会渗入 HAL、网络栈和测试工具。
以后接 LA 真板时还要再拆一次。

这就是当前 `implementation readiness = no` 的含义：不是说“永远不能实现”，也不
是说“现有文档和代码必须逐字一样”，而是说**开始写驱动之前，负责边界还没有明确到
足以避免边写边猜**。Phase 0 的目标就是把这些边界写清楚并再次审核。

## 11. 怎样分阶段，避免一次改爆

新计划不是要求一次提交全部修改。建议的顺序是：

1. **Phase 0：只修规范。** 明确资源图、设备绑定、每设备 IRQ/DMA、网络配置和
   LA 真板 hook；此阶段不写 DWMAC。
2. **Phase 1：先写防回退测试。** 让测试主动移动设备位置、调换顺序、删除资源，
   证明旧硬编码会失败。
3. **Phase 2：加入公共资源和绑定模型。** 先用主机测试覆盖零/一/多设备。
4. **Phase 3：迁移 RV QEMU。** 现有 RV QEMU 功能必须继续工作，而且移动网卡位置
   后也能工作。
5. **Phase 4：迁移 LA QEMU。** 现有 LA QEMU 功能继续工作，PCI slot 不再固定。
6. **Phase 5–6：删旧 fallback，外置网络配置。** 到这里公共路径才真正统一。
7. **Phase 7：冻结 LA 真板 hook。** 只用 synthetic provider 测接入口，不注册
   一块虚构的 LA 板。
8. **Phase 8：实现 DWMAC + StarFive glue。** 从轮询诊断开始，最后完成中断模式。
9. **Phase 9：真实 HTTPS Git 验收。** DNS、时间、CA 和证书验证全部通过后，才算
   `git clone` 完成。

每个阶段都有自己的退出条件。前一阶段失败时停在原地修，不让一半的新资源模型、
一半的旧固定路径混在一起长期存在。

## 12. 最终使用体验应该是什么样

目标不是让用户记住 MMIO 地址和 IRQ，而是只选择“我要跑哪个目标”和必要的外部
场景。具体命令名要等 xtask 接口设计阶段确定，本文不提前硬编一个尚不存在的命令。

期望体验是：

- 选择 RV QEMU：工具生成对应 QEMU 设备和网络场景，内核从固件事实绑定 VirtIO；
- 选择 LA QEMU：工具生成对应 PCI 场景，内核枚举实际 VirtIO PCI 设备；
- 选择 VF2：板级包提供 VF2 资源和 StarFive glue，网络参数来自启动配置或用户态；
- 将来选择 LA 真板：新增该板自己的 platform/driver/xtask profile 文件，不修改公共
  设备、中断、DMA、网络配置和 Git 验收代码。

“插上就完全零配置”在真板上仍取决于启动介质、串口、DHCP 和物理网络是否可用。
我们能合理做到的是：**板级差异只配置一次，普通使用者不需要每次手工改内核源码、
抄地址和重接公共逻辑。**

## 13. 三种路线的取舍

| 路线 | 第一次 VF2 联网速度 | 后续四模式维护 | 风险 |
|---|---:|---:|---|
| 只给当前 VF2 打补丁 | 最快 | 最差，LA/QEMU 还会各有一套特例 | 环境一变就出现隐蔽故障 |
| 当前选择的四模式静态公共路径 | 中等 | 较好，板级差异留在各自 provider/glue | 需要先做接口和迁移测试 |
| Linux 式完整动态设备框架 | 最慢 | 能力最强，但远超当前需求 | 工程量和复杂度过大 |

我们选择第二条。它确实比单板补丁多改一些公共接口，但明显小于完整通用设备框架，
也正好覆盖你明确需要的四种方式。

## 14. 源码证据速查

下表解释本文结论来自哪里。行号以 2026-08-04 当前工作树为准，后续修改会移动。

| 结论 | 当前证据 |
|---|---|
| `DeviceInfo` 只能表达一段 MMIO 和一个可选 IRQ | `crates/tx-hal/src/lib.rs:329-335` |
| 通用网络设备初始化直接按 `P::ARCH` 分成 RV/LA 两条 QEMU 路径 | `crates/tx-kernel/src/devices.rs:209-213` |
| RV 路径只探测 VirtIO MMIO，LA 路径使用固定 PCI 区域名 | `crates/tx-kernel/src/devices.rs:234-280` |
| 网卡固定注册为 `eth0` | `crates/tx-kernel/src/devices.rs:291-302` |
| HAL 暴露全平台单例 `NET_IRQ/net_irq()` | `crates/tx-hal/src/lib.rs:1314-1320` |
| 网络 IRQ 下半部固定按名字查 `eth0` | `crates/tx-kernel/src/irq.rs:386-393` |
| 启动网络找不到 `eth0` 时仍会选择第一项或 staging registration | `crates/tx-kernel/src/init/net.rs:143-147` |
| 启动网络内置 QEMU SLIRP 的地址、路由和静态邻居 | `crates/tx-kernel/src/init/net.rs:38-46,149-218,457-470` |
| xtask 把 RV 网卡固定到 virtio bus 1，把 LA 网卡固定到 PCI slot 2 | `xtask/src/qemu.rs:534-547` |
| active 设备规范已经把 GMAC/VirtIO 定义为启动期静态 tier-2 设备 | `docs/design/06_devices/DEVICE.md:94-147` |
| active HAL 规范要求普通驱动从 `PlatformInfo` 接收 MMIO，而非导入板级地址 | `docs/design/01_substrate/HAL_v1.md:829-887` |

### 14.1 外部规格和官方资料

- [StarFive JH7110 Datasheet：Ethernet GMAC](https://doc-en.rvspace.org/JH7110/Datasheet/JH7110_DS/ethernet_gmac.html)：
  JH7110 GMAC、RGMII 和支持的 PHY/速率事实。
- [Linux kernel stmmac documentation](https://www.kernel.org/doc/html/latest/networking/device_drivers/ethernet/stmicro/stmmac.html)：
  Synopsys Ethernet MAC 驱动、TX/RX DMA descriptor、IRQ、MDIO 和 platform data
  的成熟实现说明。
- [OASIS VirtIO 1.2 specification](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)：
  VirtIO device、virtqueue、MMIO transport 和 interrupt acknowledgement 的规范。
- [QEMU device emulation](https://www.qemu.org/docs/master/system/device-emulation.html)：
  QEMU device front end、back end、bus 和 `addr` 的含义。
- [QEMU network emulation](https://www.qemu.org/docs/master/system/devices/net.html)：
  user networking、TAP，以及默认 `10.0.2.15`/`10.0.2.2`/`10.0.2.3` 拓扑。

## 15. 名词小抄

- **QEMU**：在电脑上模拟一台机器的软件。
- **真板**：实际的 VisionFive 2 或未来 LA 开发板。
- **HAL**：把架构/板卡最底层差异包起来的接口层。
- **DT/DTB**：启动时交给内核的硬件说明书，通常描述 CPU、内存和板载设备。
- **DWMAC/GMAC**：VF2 上真实的 Ethernet MAC 控制器类型；由 DWMAC 驱动控制。
- **VirtIO-net**：QEMU 向客体机提供的标准虚拟网络设备。
- **MMIO**：CPU 用 load/store 指令访问映射到地址空间中的设备寄存器。
- **IRQ/中断**：设备向中断控制器和 CPU 报告待处理事件的请求。
- **IRQ number**：中断控制器用来区分中断来源的编号。
- **`net_irq()`**：txKernel 当前返回单个启动网卡 IRQ number 的 HAL 函数，不是
  Ethernet 或 VirtIO 标准 API。
- **DMA**：设备直接读写内存，不需要 CPU 逐字节搬运数据。
- **PHY**：以太网控制器与物理网线信号之间的芯片/电气层。
- **MDIO**：驱动配置和查询 Ethernet PHY 的管理总线。
- **RGMII**：GMAC 与千兆 Ethernet PHY 之间的数据接口。
- **virtio-mmio bus**：QEMU 中承载一个 VirtIO MMIO transport 的内部 bus 对象。
- **PCI BDF/slot**：PCI 的 Bus/Device/Function 逻辑地址；QEMU `addr=2` 指 device 2，
  不表示开发板上的第二个物理插槽。
- **BAR**：PCI 设备声明寄存器或设备内存窗口大小和位置的配置项。
- **`eth0`**：网络命名空间中的接口名称，不是驱动类型、硬件地址或 IRQ 身份。
- **IP/prefix**：网络接口的 IP 地址，以及判断同一子网所需的前缀长度。
- **route/路由**：为一个目标 IP 选择输出接口和下一跳。
- **neighbor/邻居表**：同一链路上的下一跳 IP 与 MAC 地址之间的映射。
- **SLIRP/QEMU user networking**：QEMU 在主机用户态提供的虚拟网络、NAT、DNS 和
  防火墙后端。
- **driver/驱动**：理解某类控制器寄存器和工作流程的代码。
- **glue**：把通用驱动与某个 SoC 的时钟、复位、引脚和资源连接起来的少量板级代码。
- **binder/绑定器**：把资源完整的设备实例交给能够匹配它的静态驱动。
- **top half**：中断上下文中必须快速完成的第一段处理。
- **bottom half**：离开紧急中断上下文后，对设备做确认、队列扫描和网络唤醒的处理。
- **fallback**：主路径缺失时使用的备用实现；如果它伪装硬件成功，反而会掩盖错误。
- **hook/seam**：预先定义好的接入口。LA 真板 hook 只规定“以后从这里接”，不假装
  已经知道那块板的硬件事实。

## 16. 读完后只需要记住四句话

1. 之前 clone 不通，主要不是 Git 问题，而是 VF2 真实网卡驱动路径缺失。
2. 旧方案能抢修一块板，但容易把当前板和当前网络的数值带进公共代码。
3. 新方案会触及多个层次，却不重写 Git、VFS、TCP/IP、调度器或整个内核。
4. 下一步仍然是 Phase 0 的文档和接口边界，不是马上大规模写 DWMAC 驱动。
