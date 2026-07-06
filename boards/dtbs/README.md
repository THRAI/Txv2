# 设备树测试夹具（P1 设备探测化的单测固定资产）

这些 `.dtb` 是**真实环境的设备树快照**，供板级 HAL 的 host 单元测试直接解析并断言设备发现结果（uart/plic/virtio/sdio 节点、PLIC context 推导等）。用途和来源：

| 文件 | 来源 | 导出方式 |
|---|---|---|
| `qemu-rv64-virt.dtb` | QEMU 9.2.1（本仓库评测同款，`qemu-local/install-9.2.1`） | `qemu-system-riscv64 -machine virt,dumpdtb=… -m 256M -smp 4`（参数与 xtask busybox profile 一致） |
| `qemu-la64-virt.dtb` | 同上 | `qemu-system-loongarch64 -machine virt,dumpdtb=… -cpu la464 -m 1152M -smp 4` |
| `jh7110-starfive-visionfive-2-v1.3b.dtb` | Chronix 仓库 `hal/src/board/dtbs/`（原始出处为 Linux 内核 `arch/riscv/boot/dts/starfive/`，GPL-2.0/MIT 双许可的 dts 编译产物） | 直接复制；与我们的 VF2 板实测版本（V1.3B）一致 |

注意：
- QEMU 的 dtb 随启动参数变化（`-m`/`-smp`/挂的设备），重新导出时必须使用与 xtask 一致的参数和同版本 QEMU；
- `dumpdtb` 导出的是 QEMU 生成的原始树，OpenSBI 启动时会在其上追加自己的 reserved-memory 节点后才传给内核——单测断言设备节点不受影响，但不要用它断言保留内存；
- 内核运行时**不使用**这些文件（动态路线：固件经 a1/EFI 传入）；未来 LA 真板的嵌入式兜底 dtb 另行管理。

维护：2026-07-02 建立（P1.0），见 `ljs/07-P1设备探测化设计.md`。

- ls2k1000-dp-v10.dts — 龙芯2K1000-DP-V10 真板设备树(文本,取自 Del0n1x 仓库 ls2k1000_dp.dts.out)。实锤:serial0=0x1fe20000@115200n8(stdout-path)、ethernet@40040000/40050000 双网口。无 memory 节点(固件运行时注入)。
