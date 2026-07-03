# tools/ 备注

多数脚本各自内嵌用途说明;此文件只登记外来二进制的出处。

- `mkimage-loongarch`:U-Boot mkimage v2022.04(GPL-2.0),收编自
  NPUcore-BLOSSOM(决赛仓库 T202510699995276-827 的 `util/mkimage`)。
  发行版 u-boot-tools 的 mkimage 不认识 LoongArch;此版本用于
  `cargo xtask image la-uimage` 打包 LS2K1000 的内核/initrd uImage。
