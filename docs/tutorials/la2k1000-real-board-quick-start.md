# Loongson 2K1000 实板快速启动

这是一份可以直接照着做的最短流程：构建镜像、放入 TFTP、截停 U-Boot、
以只读 ext4 根文件系统启动、配置网络，并完成 ping 和 Git 验证。

默认使用只读根文件系统。最后一节单独说明如何做一次**只新增文件**的持久化
验证；第一次启动时不要直接跳到那一节。

## 0. 安全规则

- 所有宿主机命令都在 `/home/msp/learning/Txv2` 执行。
- 每次向 TFTP 目录复制时使用一个新文件名，不覆盖旧镜像。
- U-Boot 中不要运行 `saveenv`，不要执行 `sf write`、`mmc write`、`dd`、
  `mkfs`、分区或修复文件系统的命令。
- 普通启动固定使用 `tx.root=sda1 ro`。
- 如果任何设备型号、磁盘容量、镜像校验或 bootargs 不符合下文，立即停止。

## 1. 构建镜像

```bash
cd /home/msp/learning/Txv2
cargo xtask build --target la64-2k1000 --kernel-only --release
cargo xtask image la2k1000-uimage --kernel-only --release
```

生成的镜像是：

```text
/home/msp/learning/Txv2/target/images/txv2-la2k1000.uimage
```

## 2. 复制到 TFTP 目录

下面以 `txv2-la2k1000-mytest-01.uimage` 为例。下一次测试请把 `mytest-01`
改成一个新名字。

```bash
# 进入 txKernel 当前工作树
cd /home/msp/learning/Txv2

# 检查 TFTP 目录中是否已经存在同名镜像
# 没有输出表示文件不存在，可以继续；如果已存在，请换一个新文件名
sudo test ! -e /srv/tftp/txv2/txv2-la2k1000-mytest-01.uimage

# 把刚构建的镜像复制到 TFTP 目录
# --no-clobber 表示绝不覆盖已经存在的文件
sudo cp --no-clobber target/images/txv2-la2k1000.uimage \
  /srv/tftp/txv2/txv2-la2k1000-mytest-01.uimage

# 逐字节比较源镜像和 TFTP 镜像
# 没有任何输出表示两个文件完全一致
cmp target/images/txv2-la2k1000.uimage \
  /srv/tftp/txv2/txv2-la2k1000-mytest-01.uimage

# 分别计算两个镜像的 SHA-256
# 输出的两个哈希值必须完全相同
sha256sum target/images/txv2-la2k1000.uimage \
  /srv/tftp/txv2/txv2-la2k1000-mytest-01.uimage
```

`cmp` 必须没有输出，两个 SHA-256 必须相同。

如果只想复用已经通过实板验证的镜像，可以直接使用：

```text
/srv/tftp/txv2/txv2-la2k1000-pr-460152ced-oldworldsig-3fd30c0d.uimage
```

它的 SHA-256 是：

```text
3fd30c0d96dffc9ffcc65b4ec2aa332de467185342c6ee433e5c5f912c535914
```

## 3. 打开串口并截停 U-Boot

这块板的 U-Boot 等待时间接近 0 秒，因此要先打开串口，再一边连续按小写字母
`c`，一边复位开发板：

```bash
# 进入当前工作树
cd /home/msp/learning/Txv2

# 打开 /dev/ttyUSB0，波特率 115200，并关闭串口流控
sudo picocom -b 115200 --flow n /dev/ttyUSB0
```

打开串口后的操作顺序：

1. 开始持续、快速地按小写字母 `c`。
2. 保持按 `c`，同时复位开发板。
3. 看到 U-Boot 提示符 `=>` 后停止按键。
4. 按一次 `Ctrl-C` 清除可能残留的 `cccc`，再按一次回车。
5. 从下一节的 `scsi reset` 开始输入 U-Boot 命令。

如果直接进入厂商 Linux，说明这次没有截停成功。不要在厂商 Linux 中输入
命令；再次复位并重新尝试。退出 picocom 时先按 `Ctrl-A`，再按 `Ctrl-X`。

## 4. 在 U-Boot 中加载镜像

先检查磁盘：

```text
scsi reset
```

必须看到 Kingchuxing 32GB、`62533296 x 512`。不一致就停止，不要启动 RW。

然后逐行执行：

```text
setenv ipaddr 192.168.1.20
setenv serverip 192.168.1.2
setenv ethact ethernet@40050000
tftpboot 0x9000000097ffffc0 txv2/txv2-la2k1000-mytest-01.uimage
iminfo 0x9000000097ffffc0

fdt addr ${fdtcontroladdr}
fdt move ${fdtcontroladdr} 0x900000000a000000 10000
fdt addr 0x900000000a000000
setenv fdt_addr 0x900000000a000000
setenv fdt_high 0xffffffffffffffff

setenv bootargs tx.profile=alpine tx.board=ls2k1000
setenv bootargs ${bootargs} console=ttyS0 tx.root=sda1
setenv bootargs ${bootargs} ro tx.mount.sdcard=0
setenv bootargs ${bootargs} init=/bin/sh
fdt set /chosen bootargs ${bootargs}

printenv bootargs
fdt print /chosen bootargs
bootm 0x9000000097ffffc0
```

`iminfo` 必须显示镜像校验成功。执行 `bootm` 前，最后两条输出必须同时包含
`tx.root=sda1` 和 `ro`。这里故意只给 `bootm` 一个参数，不加载 initrd。

如果使用上面的已验证镜像，只需把 `tftpboot` 行中的文件名替换成
`txv2-la2k1000-pr-460152ced-oldworldsig-3fd30c0d.uimage`。

## 5. 检查启动是否成功

串口应看到以下关键行：

```text
txkernel:ahci:probe:ok:blocks=62533296:block-size=512
txkernel:dwmac3:probe:ok
txkernel:mount:rootfs:ext4:sda1:ok
txkernel:boot:ok
```

进入 `sh-5.0#` 后检查根文件系统：

```sh
/bin/busybox mount
```

必须看到：

```text
/dev/sda1 on / type ext4 (ro)
```

如果显示 `rw`、根是 tmpfs，或者 AHCI/DWMAC probe 失败，立即停止。

## 6. 配置板端网络

先查看网卡名：

```sh
/bin/busybox ip link
```

忽略 `lo`。下面假设实际网卡名是 `eth0`；如果板上显示 `eth1`，把下面三条
命令里的 `eth0` 换成 `eth1`。

```sh
/bin/busybox ip link set eth0 up
/bin/busybox ip addr add 192.168.1.27/24 dev eth0
/bin/busybox ip route add default via 192.168.1.2 dev eth0
/bin/busybox ip addr show eth0
/bin/busybox ip route
```

## 7. 验证网络和 Git

先只验证板子到宿主机的局域网链路：

```sh
/bin/busybox ping -c 3 192.168.1.2
/bin/busybox ping -q -i 0.05 -c 600 192.168.1.2
```

期望结果是 3/3、600/600，丢包率 0%。

如果还要访问 GitHub，宿主机需开启转发。当前实验机使用 `eth0` 接板子、
`Mihomo` 出公网；如果这些规则已经存在，就不要重复添加：

```bash
sudo sysctl -w net.ipv4.ip_forward=1

sudo iptables -C FORWARD -i eth0 -o Mihomo -s 192.168.1.27/32 \
  -m conntrack --ctstate NEW,ESTABLISHED,RELATED \
  -m comment --comment txv2-la-git -j ACCEPT 2>/dev/null || \
sudo iptables -I FORWARD 1 -i eth0 -o Mihomo -s 192.168.1.27/32 \
  -m conntrack --ctstate NEW,ESTABLISHED,RELATED \
  -m comment --comment txv2-la-git -j ACCEPT

sudo iptables -C FORWARD -i Mihomo -o eth0 -d 192.168.1.27/32 \
  -m conntrack --ctstate ESTABLISHED,RELATED \
  -m comment --comment txv2-la-git -j ACCEPT 2>/dev/null || \
sudo iptables -I FORWARD 1 -i Mihomo -o eth0 -d 192.168.1.27/32 \
  -m conntrack --ctstate ESTABLISHED,RELATED \
  -m comment --comment txv2-la-git -j ACCEPT

sudo iptables -t nat -C POSTROUTING -s 192.168.1.27/32 -o Mihomo \
  -m comment --comment txv2-la-git -j MASQUERADE 2>/dev/null || \
sudo iptables -t nat -A POSTROUTING -s 192.168.1.27/32 -o Mihomo \
  -m comment --comment txv2-la-git -j MASQUERADE
```

局域网 ping 只证明网卡、地址和到宿主机的链路正常，并不会自动配置 DNS。
当前实验网络验证过的上游 DNS 是 `10.248.98.30`。在宿主机先确认它仍是当前
上游 DNS：

```bash
resolvectl dns
ip route get 10.248.98.30
```

如果宿主机没有 `resolvectl`，可以查看：

```bash
nmcli dev show | grep IP4.DNS
```

不要把宿主机的 `127.0.0.53` 写到板端；它只在宿主机本地有效。也不要把
`192.168.1.2` 当作 DNS，除非宿主机确实运行了监听该地址的 DNS 服务。

回到板端，先检查现有 resolver：

```sh
ls -l /etc/resolv.conf
cat /etc/resolv.conf
```

如果已经包含可达的 `nameserver 10.248.98.30`，无需修改。如果缺失或内容错误，
需要在 **RW 根文件系统**中备份并修正：

```sh
test ! -e /root/tx-network-config-01
mkdir /root/tx-network-config-01
cp -L /etc/resolv.conf /root/tx-network-config-01/resolv.conf.before
printf 'nameserver 10.248.98.30\n' > /etc/resolv.conf
cat /etc/resolv.conf
```

如果 `cat` 显示 `/etc/resolv.conf` 原本不存在，跳过 `cp`，直接创建新文件。上述
操作只修改 `sda1` 上的 resolver 配置，不接触 U-Boot/Flash；备份目录必须是从未
使用过的新名字。如果当前根是 RO，不要强行 remount：先跳过 Git，按第 8 节
重启到 RW 配置一次，再物理复位回 RO 验证。

然后依次验证公网路由和 DNS：

```sh
ping -c 3 8.8.8.8
nslookup github.com
```

TLS 不需要单独“开启”，但必须同时具备正确的 UTC 时间和 CA 根证书。这块板没有
可依赖的持久 RTC，因此每次复位后都要重新检查时间。在宿主机生成一条当前 UTC
设置命令：

```bash
date -u '+date -u -s "%Y-%m-%d %H:%M:%S"'
```

把它打印出的整条 `date -u -s "..."` 命令复制到板端执行，然后检查 CA：

```sh
date -u
ls -l /etc/ssl/cert.pem
ls -l /etc/ssl/certs/ca-certificates.crt
```

两个 CA 路径至少有一个必须存在且可读；如果都不存在，停止 HTTPS 测试，不要使用
`GIT_SSL_NO_VERIFY=1` 或其他方式关闭证书校验。

最后验证 TLS/HTTPS Git：

```sh
/root/txv2-460152ced-20260814-c/toolroot/bin/git \
  ls-remote https://github.com/LLLPPPS/tx-push-test.git
```

如果这个 `toolroot` 路径不存在，先停止 Git 步骤；不要临时覆盖系统目录里的工具。

最后把仓库 clone 到内存文件系统，避免这一步写根盘：

```sh
/bin/busybox test ! -e /dev/shm/tx-git-net-test-01
/root/txv2-460152ced-20260814-c/toolroot/bin/git clone --depth=1 \
  https://github.com/LLLPPPS/tx-push-test.git \
  /dev/shm/tx-git-net-test-01
/root/txv2-460152ced-20260814-c/toolroot/bin/git \
  -C /dev/shm/tx-git-net-test-01 fsck --full --no-progress
```

clone 完成、`git fsck` 返回 0、shell 提示符重新出现，才算网络与 Git 全链路通过。

## 8. 可选：验证 ext4 持久化

只有只读启动和局域网 ping 通过后，才做这一节。如果 RO 启动时 resolver 已可用，
应先完成内存中的 Git clone；如果 resolver 尚未配置，则在本次 RW 启动中先按
第 7 节完成 DNS、UTC、CA 和 `git ls-remote` 门禁，再进行持久化 clone。

1. 物理复位，再次用第 3 节的 picocom 手动按 `c` 截停 U-Boot。
2. 重复第 4 节，但把 bootargs 中的 `ro` 那一行替换为：

   ```text
   setenv bootargs ${bootargs} rw tx.ext4.rw-profile=legacy-nocsum tx.mount.sdcard=0
   ```
3. 启动后确认 `/dev/sda1 on / type ext4 (rw)`。
4. 只使用一个从未存在过的新目录，不删除、不覆盖任何旧路径：

   ```sh
   /bin/busybox test ! -e /root/tx-persist-test-01
   /bin/busybox mkdir /root/tx-persist-test-01
   /root/txv2-460152ced-20260814-c/toolroot/bin/git clone --depth=1 \
     https://github.com/LLLPPPS/tx-push-test.git \
     /root/tx-persist-test-01/repo
   /root/txv2-460152ced-20260814-c/toolroot/bin/git \
     -C /root/tx-persist-test-01/repo fsck --full --no-progress
   ```
5. 所有命令返回且串口稳定后，使用物理复位；不要运行 BusyBox `reboot`。
6. 再按第 4 节以 `ro` 启动，并验证：

   ```sh
   /root/txv2-460152ced-20260814-c/toolroot/bin/git \
     -C /root/tx-persist-test-01/repo rev-parse HEAD
   /root/txv2-460152ced-20260814-c/toolroot/bin/git \
     -C /root/tx-persist-test-01/repo fsck --full --no-progress
   ```

复位后的只读启动仍能读到同一个 HEAD，且完整 `git fsck` 通过，才算持久化闭环。

## 9. 最常见的失败

- **直接进入厂商 Linux**：没有截停 U-Boot。不要在厂商 Linux 中执行命令；再次
  复位并使用自动截停脚本。
- **TFTP 超时**：检查宿主机地址是否为 `192.168.1.2`、文件是否位于
  `/srv/tftp/txv2/`，以及 U-Boot 中的文件名是否一致。
- **能 ping 宿主机但 GitHub 不通**：优先检查宿主机转发/NAT 和 DNS，不要先怀疑
  DWMAC 驱动；DNS 通过后再检查 UTC 和 CA，禁止关闭 TLS 校验。
- **根文件系统不是 ext4 或不是 ro**：停止测试，不要继续 Git 或写盘。

完整实板证据与故障分析保存在
[2026-08-15 LA 持久 Git/网络/SIGALRM 记录](../../msp/debug-logs/2026-08-15-main-merge-la-persistent-git-network-sigalrm.md)。
