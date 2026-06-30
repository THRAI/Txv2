![校徽](docs/image/校徽.jpg)

# txKernel

## 项目简介

- txKernel是一个使用Rust实现、支持RISC-V64和LoongArch64硬件平台的多核操作系统内核。
- 执行模型采用**异步无栈协程**：每个线程是一个future，由reactor调度，内核不为线程保留独立内核栈。
- 内核分为**substrate（基座）**与**语义子系统**两层；同一份内核源码经编译期平台选择即可在两种架构上运行，兼容Linux ABI。

## 完成情况

### 排名情况

### 初赛情况

截至6月30日15点，TxKernel已经通过初赛的大部分测试点，在排行榜上排在第12位：

![board-rank](./docs/image/排名.png)

得分详情如下：

![](./docs/image/得分.png)

### 内核介绍

- **进程管理**：异步无栈协程、多核调度；进程/线程按identity/payload拆分；支持fork/clone/exec/exit/wait。
- **内存管理**：映射的权威描述（recipe）与派生页表（pmap）分离、无锁并发；按需分页、写时复制、懒分配；物理页分配器+内核堆+对象池（zone）+EBR延迟回收。
- **文件系统**：统一文件身份RNode+多种后端；DEntry路径缓存与挂载（含bind/传播）；页缓存与mmap共享同一物理页；支持ext4，以及tmpfs/procfs/devfs。
- **进程间通信**：信号（标准+实时）、管道、futex、System V IPC（信号量/消息队列/共享内存）、eventfd/signalfd/timerfd/epoll。
- **中断与异常**：统一trap路径，平台侧/内核侧两层+TrapAction跨架构复用；无栈协程上下文切换。
- **设备驱动**：virtio-blk/virtio-net、串口与TTY（行规程）；静态设备模型+设备树（FDT）解析。
- **网络模块**：基于smoltcp的TCP/UDP，支持IPv4/IPv6与本地回环。
- **硬件抽象层**：axHal风格的静态平台族，编译期选定平台，无运行时HAL管理器。
- **应用支持**：支持busybox等现实应用，通过OSComp basic、libc-test、LTP等测试。

<img src="docs/image/_txKernel的核心设计.png" alt="TxKernel 内核架构" width="500"/>

### 文档

- [初赛技术报告](./TxKernel内核初赛文档.pdf)
- [项目开发简介幻灯片](./TxKernel初赛ppt.pdf)
- [演示视频](https://pan.baidu.com/s/1bsYjpYtcXR_GtTfcPMTy9g) 提取码：1234

### 项目结构

```
.
├── boards/             # 板级HAL与内核二进制crate（RISC-V/LoongArch·qemu-virt）
├── crates/             # 架构无关的内核crate
│   ├── tx-drivers/          # virtio、串口/TTY驱动
│   ├── tx-ext4/             # ext4文件系统
│   ├── tx-ext4-format/      # ext4镜像格式化
│   ├── tx-fat/              # FAT文件系统
│   ├── tx-fat-format/       # FAT镜像格式化
│   ├── tx-fs/               # 文件系统框架与tmpfs/procfs/devfs/devpts
│   ├── tx-hal/              # 硬件抽象层
│   ├── tx-kernel/           # 内核装配：启动、trap分发、初始化
│   ├── tx-observe/          # 内核侧观测/追踪运行时
│   ├── tx-observe-types/    # 观测数据类型定义
│   ├── tx-platform-adapter/ # 平台边界适配宏（#[platform_adapter]）
│   ├── tx-policy/           # 调度与cgroup策略
│   ├── tx-reactor/          # 无栈协程reactor与调度
│   ├── tx-scripts/          # 操作流程编排（drive、进程、挂载、路由）
│   ├── tx-services/         # 内核服务：随机数、凭证、rlimit、时间、trace
│   ├── tx-shims/            # Linux系统调用语义与ABI适配
│   ├── tx-substrate/        # 基座：对象池（zone）、EBR、索引、发布总线、帧、预留
│   ├── tx-subsystems/       # 语义子系统：进程、内存、文件系统、IPC、网络、设备、信号
│   ├── tx-test-support/     # 测试支撑：step引擎、宿主驱动
│   └── tx-vdso/             # 编译期vDSO ELF镜像
├── docs/               # 设计文档与开发记录
├── external/           # 第三方依赖与测试套件（musl、lmbench、oscomp-autotest等）
├── tools/              # 评测、构建与调试脚本（oscomp-judge、LTP运行器等）
└── xtask/              # 统一开发命令（构建/QEMU/OSComp）
```

## 运行方式


### 编译

在项目根目录运行，同时构建RISC-V64与LoongArch64内核：

```bash
make all
```

### 运行

```bash
make oscomp-local-rv64     # 启动 RISC-V 内核并本地评测
make oscomp-local-la64     # 启动 LoongArch 内核并本地评测
```

## 项目人员

- 杨岩琰（队长）：负责reactor设计，线程进程设计及VFS设计。
- 刘佳硕：负责硬件抽象层设计、RISC-V与龙芯设计，TTY设计。
- 孟书培：负责网络栈设计、网络外设驱动设计。
- 指导老师：夏文，仇洁婷

## 参考

- **ArceOS**（[axhal](https://arceos.org/arceos/axhal/index.html)）—— 对其axhal进行修改实现了我们的硬件抽象层。
- **StarryOS**（[rsext4](https://github.com/Starry-OS/rsext4)）—— 抛弃其内部缓存，提取同步逻辑对其做了异步适配。
- **Asterinas**（[仓库](https://github.com/asterinas/asterinas)）—— 参考其网络栈设计。
- **《FreeBSD操作系统设计与实现（第二版）》与FreeBSD内核代码**（[仓库]("https://cgit.freebsd.org/src/")） —— 最初的学习资源，提供了第一版内核架构参考，以及第二版内核的消息总线设计。
- **Chronix**（[仓库](https://gitlab.eduxiji.net/educg-group-36002-2710490/T202518123995568-675)）—— 无栈异步协程设计。
- **Linux**（[官网](https://www.kernel.org/)）—— 大量参考，系统调用功能的金标准。