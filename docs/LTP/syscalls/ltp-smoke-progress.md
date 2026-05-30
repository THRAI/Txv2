# LTP Smoke Progress

记录当前 `ltp-musl` smoke 批次的通过情况。运行命令：

```bash
timeout 300s make oscomp-local-rv64-ltp-batch LTP_BATCH=smoke
```

2026-05-25 完整 smoke 跑分：`131/171`，共 33 个 case。这个批次已按 p0 的方式在 guest 侧展开，主机侧只传短命令 `ltp-batch:smoke`，避免长 cmdline 截断。

2026-05-29 在 Docker 不可用的本地环境下，用已有 `target/oscomp/submit/kernel-rv`
和完整 `target/oscomp/testdata/sdcard-rv.img` 直接运行：

```bash
timeout 300s cargo xtask oscomp qemu --target rv64-qemu \
  --data target/oscomp/testdata --submit target/oscomp/submit \
  --suite ltp-batch:smoke
python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv.txt target/oscomp/testdata
```

结果为 `135/172`，串口快照保存为
`target/oscomp/os_serial_out_ltp_smoke_fullimage_20260529_203658.txt`。同一轮也
确认了两个本地执行 caveat：`make oscomp-local-rv64-ltp-batch` 仍依赖 Docker；
使用 `target/oscomp/tailor/smoke` 这类旧 slim image 会让所有 case 以
`sh: <case>: not found` / `127` 失败，不能作为 kernel coverage。

当前判断：smoke 剩余未过项暂时不作为主线优先项。剩下的问题大多不是小范围 errno/flag 修补：

- 环境或 libc 非 POSIX 接口缺失：`getcontext01`、`gethostid01`、`mallinfo02`、`mallinfo2_01`、`mallopt01`、`profil01`。
- 需要较大模块语义：`realpath01` 涉及 `chroot`，`pathconf02` 涉及 `/etc/passwd`/`nobody` 这类用户数据库环境。
- 需要单独定位但短期收益不确定：`fmtmsg01`、`qmm01`、`switch01` 可能是 runtest 命令映射或二进制/别名问题；`nftw01`、`nftw6401` 需要看目录遍历细节；`gethostname02` 牵到 libc `gethostname()` 截断行为。
- 部分通过但仍有缺口：`nftw01`、`nftw6401`、`pathconf02`、`sysconf01`。

后续建议：smoke 保持记录为 `135/172`，优先推进 `fd-io` 或 `vfs`，这两批更可能带来成片修复收益。

| Area | Case | Status | Score | Note |
| --- | --- | --- | --- | --- |
| libc | abort01 | PASS | 2/2 | 2026-05-29 full-image smoke 确认 |
| libc | confstr01 | PASS | 32/32 |  |
| libc | fmtmsg01 | FAIL | 0/1 | exit 127，疑似缺少二进制/命令映射 |
| libc | fpathconf01 | PASS | 9/9 |  |
| libc | getcontext01 | DEFER | 0/1 | `getcontext` unsupported |
| libc | gethostbyname_r01 | FAIL | 0/1 | 返回值不是期望的 `ERANGE` |
| libc | gethostid01 | DEFER | 0/1 | `sethostid` undefined |
| libc | gethostname01 | PASS | 1/1 |  |
| libc | gethostname02 | FAIL | 0/1 | 短 buffer 应失败但成功 |
| libc | getpagesize01 | PASS | 1/1 |  |
| random | getrandom01 | PASS | 4/4 |  |
| random | getrandom02 | PASS | 4/4 |  |
| random | getrandom03 | PASS | 9/9 |  |
| random | getrandom04 | PASS | 1/1 |  |
| random | getrandom05 | PASS | 2/2 | 单测确认 |
| malloc | mallinfo02 | DEFER | 0/1 | non-POSIX `mallinfo` unsupported |
| malloc | mallinfo2_01 | DEFER | 0/1 | non-POSIX `mallinfo2` unsupported |
| malloc | mallopt01 | DEFER | 0/1 | non-POSIX `mallopt` unsupported |
| string | memcmp01 | PASS | 2/2 |  |
| string | memcpy01 | PASS | 2/2 |  |
| string | memset01 | PASS | 1/1 |  |
| fs | nftw01 | PARTIAL | 2/3 | 需单独看目录遍历细节 |
| fs | nftw6401 | PARTIAL | 2/3 | 需单独看目录遍历细节 |
| fs | pathconf01 | PASS | 17/17 |  |
| fs | pathconf02 | PARTIAL | 1/6 | `getpwnam(nobody)`/用户数据库环境相关 |
| libc | profil01 | DEFER | 0/2 | unsupported/TCONF |
| vm | qmm01 | UNKNOWN | 0/0 | judge 未计分，runtest 映射到 `mmap001 -m 1` |
| fs | realpath01 | FAIL | 0/1 | `chroot` ENOSYS |
| string | string01 | PASS | 1/1 |  |
| libc | switch01 | UNKNOWN | 0/0 | judge 未计分，runtest 映射到 `endian_switch01` |
| syscall | syscall01 | PASS | 3/3 |  |
| libc | sysconf01 | PARTIAL | 36/56 | 仍有部分 `_SC_*` 语义缺失或返回值不符 |
| libc | ulimit01 | PASS | 3/3 |  |
