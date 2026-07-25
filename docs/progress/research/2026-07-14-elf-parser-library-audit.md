# ELF 解析器与候选库审计

日期：2026-07-14
快照：**2026-07-14 initial snapshot (historical)**
状态：以下“结论”至“验证与后续”只描述 2026-07-14 当日 checkout；当时
只读审计且未修改解析实现或 Cargo 依赖。当前实现状态从
“2026-07-15 implementation update”开始。

## 结论

当前 exec 路径使用的是 `goblin 0.10.5`，而非手写字节解码：
`crates/tx-scripts/src/process/exec/loader.rs::parse_image_plan` 将 ELF
header/program headers 变成 txKernel 自有的 `ExecImagePlan`，随后由
`exec_script` 把它映射到 VM recipe。保留这个 txKernel-owned plan
边界是正确的。

若要替换底层 parser，应优先评估 Rust `elf` crate 0.8.0；不要采用
`vincenthouyi/elf_rs`。但替换库不是当前最高优先级：当前 loader 的
输入读取分段和策略校验有独立问题，任何通用 ELF crate 都不会替它补上。

## 当前路径与验证

| 环节 | 证据 | 结论 |
|---|---|---|
| 依赖 | `crates/tx-scripts/Cargo.toml:14-24` | 使用 `goblin 0.10.5`，为其 `elf64` feature-gating 缺陷临时启用 `elf32`。 |
| 纯解析边界 | `crates/tx-scripts/src/process/exec/loader.rs:189-355` | `goblin` 类型未逃逸；输出为自有 `ExecImagePlan`。 |
| 输入来源 | `crates/tx-scripts/src/process/exec/script.rs:513-536` | 只读取 `min(file_size, 4096)` 字节后同时解析 ELF header 与 program-header table。 |
| EOF 防线 | `crates/tx-subsystems/src/page_backed/targeted_read.rs:35-62` | `read_exact_at` 对每一次已发起的读取正确做溢出、EOF 和容量检查。 |
| 已有单测 | `crates/tx-scripts/src/process/exec/loader/tests.rs` | `cargo test -p tx-scripts process::exec::loader::tests --lib`：23/23 通过。 |

## 发现

1. **P1：program-header table 的读取协议把合法 ELF 限制为前 4 KiB。**
   设计要求先读 64-byte header，再按 `e_phoff` 和已验证长度单独读取
   program-header table（`docs/design/02_execution/EXEC_v1.md:488-490`）。
   当前实现把 table 直接限制在已读取 window 中
   (`loader.rs:245-256`)，因此 `e_phoff > 4096` 的合法、受上限约束的
   ELF 会得到 `ENOEXEC`。应先解析 header，再以 `read_exact_at` 发起
   最多 `64 * 56` bytes 的第二次读取。

2. **P1：`PT_INTERP` 的内容读取不满足不可信输入边界。**
   `loader.rs:266-277` 直接以 `p_offset as usize` 和 `off + len` 在
   header window 内切片：它既不能读取位于 window 外的有效解释器路径，
   也没有 checked-add，极端 `p_offset` 会触发整数溢出/越界 panic。
   这会使带 `PT_INTERP` 的 `ET_DYN` 可能在路径未取得时继续按无解释器
   形状执行。应先对 `p_offset + p_filesz` 做 `u64` checked-add 并以文件
   长度验证，再单独、有上限地读取该字符串，并要求非空、NUL 终止和
   绝对路径。

3. **P1：机器类型与用户虚拟地址策略没有在 loader 边界落地。**
   文档要求 `e_machine` 匹配当前 build target，以及
   `load_bias + p_vaddr + p_memsz <= USER_TOP`
   (`EXEC_v1.md:623-650`)。但 loader 接受 RV64 与 LA64 两种
   `e_machine` (`loader.rs:222-224`；对应 LA64 接受测试在
   `loader/tests.rs:443-450`)，且只检查未加 bias 的 address overflow
   (`loader.rs:369-375`)。后续 `UserRange::new_aligned` 也只检查对齐和
   `usize` overflow，不检查 `FULL_USER_V1_TOP`
   (`crates/tx-subsystems/src/vm/structure/types.rs:88-101`)。应把静态
   platform 的允许 machine 和 user-VA top 传给 loader，并在每个
   `PT_LOAD` 的 page-rounded final range 上拒绝越界。

4. **P2：代码与 active exec 规范的策略表不一致。**
   规范要求 `PT_INTERP` 和 executable `PT_GNU_STACK` 返回 `ENOEXEC`
   (`EXEC_v1.md:634,655`)；实现已经尝试 PT_INTERP 动态加载并仅记录
   GNU stack 的 executable bit (`loader.rs:264-306`)。此外规范允许
   `p_align` 为 0/1 或 page-size 以下的 powers of two，而实现要求
   `p_align >= PAGE_SIZE` (`loader.rs:377-383`)。应先确定“动态 exec 已
   升格为正式契约”还是回退到 v1 拒绝策略，然后同步规范、实现、测试；
   不能让两个实现语义同时存在。

5. **P2：缺少若干语义回归测试。**
   现有 23 个 hand-crafted fixture 覆盖了常见 header、BSS、W^X 与
   overlap 情形，但没有覆盖非零且 window 外的 `e_phoff`、window 外或
   超大 `PT_INTERP`、跨 ISA `e_machine`、USER_TOP、`e_version`、entry
   是否位于 executable `PT_LOAD`、以及 `PT_PHDR` 是否确实描述已加载的
   program-header table。先为这些策略加回归测试，再考虑换 parser。

## 候选库

| 库 | 观察 | 对 txKernel 的判断 |
|---|---|---|
| [`elf` 0.8.0](https://docs.rs/elf/latest/elf/) | `no_std`、zero-alloc、显式 `AnyEndian`/`LittleEndian`，`ElfBytes::minimal_parse(&[u8])` 只解析 header 并延迟解析 segment/section table；公开文档声明仅用 safe interfaces，且有 fuzz coverage。 | **推荐作为 goblin 的替代候选。** 它符合内核 parser 的内存与端序要求，能去掉当前为 goblin feature bug 保留的 `elf32` workaround。仍需在 adapter 中保留所有上述 txKernel policy 校验与分段 I/O；它不是 exec loader。 |
| [`vincenthouyi/elf_rs`](https://github.com/vincenthouyi/elf_rs) 0.3.1 | `no_std`，但仓库 HEAD `9f77869`（2023-10-12）以 `unsafe` 把 `&[u8]` 转成 `&ElfHeader` / `&[ProgramHeader]` (`src/elf/elf.rs:72-82`)；字段通过 native `read_unaligned` 读取而没有按 `EI_DATA` 转换 (`src/elf_header/elf_header.rs:52-101`)；`from_bytes` 只验证长度、magic、class。 | **不推荐。** 它的 raw-reference/alignment/endianness 信任模型不适合作为内核处理不可信 executable 的可信边界，且比现有 goblin 或 `elf` 提供更少的校验能力。 |

## 建议落地顺序

1. 把读取重构为 `header -> bounded phdr table -> bounded PT_INTERP string` 三段；不要把 `&[u8]` parser API 误当成文件 I/O 协议。
2. 在 txKernel adapter 固定策略：target ISA、final user VA range、entry 位于 executable LOAD、`PT_PHDR` 可证明地对应已加载 table、`PT_INTERP` 语义和 GNU_STACK policy。
3. 为上述发现补 unit/property/fuzz regression；将真实 RV64、LA64、static PIE、dynamic-linker ELF fixture 加入兼容性集。
4. 只有策略测试锁定后，做一个小型 `elf::ElfBytes<LittleEndian>` adapter spike，与 goblin 对同一 fixtures 的 header/PT_LOAD 结果做 differential comparison；通过后再移除 goblin workaround。

## 验证与后续

- 已运行：`cargo test -p tx-scripts process::exec::loader::tests --lib`，23 passed。
- 未运行 QEMU：本次未改动行为，审计结论来自当前 source、设计规范和上游公开源码。
- 后续 blocker：动态 `PT_INTERP` 已进入实现，但 active `EXEC_v1` 仍描述为 v1 拒绝，必须由 exec/VM owner 先选择正式语义。

## 2026-07-15 implementation update

审计建议已经进入生产路径。`tx-scripts` 现在以
`ElfFileParser` 作为可替换的纯语法解析边界，由 `Elf08Parser` 使用
`elf` 0.8.0 的低层 header/`ParsingTable` API 实现；第三方类型不会越过
Tx 自有的 `ElfHeader`、`ElfProgramHeader` 和 `ExecImagePlan` 边界。
`goblin` 及其为 `elf32` feature-gating 保留的 workaround 已从生产依赖、
测试依赖和 lockfile 退役。

文件输入现按 `64-byte header -> bounded phdr table -> bounded PT_INTERP`
分段读取。`ElfLoadPolicy` 固定目标 ISA、page size、`USER_TOP`、最多 64 个
program headers 和解释器策略；Tx 侧继续负责 LOAD/entry/PHDR/final VA、
TLS、DYNAMIC、RELRO、GNU_STACK 元数据和 main/interpreter/stack/vDSO 组合
布局。动态解释器作为第二镜像验证，`AT_ENTRY` 指向 main，`AT_BASE` 提供
interpreter load bias；内核仍不处理 dynamic tags、符号、依赖或重定位。

2026-07-15 follow-up hardening keeps staged-read allocation and I/O failures
role-accurate: phdr/`PT_INTERP` buffers use fallible reservation, and PageBacked
`ENOMEM` remains `ENOMEM` for both main and interpreter reads. Main and
interpreter now share one executable-candidate path that requests neither read
nor write access, accepts execute-only regular files, checks execute bits, and
requires a PageBacked terminal RNode. VFS child publication now returns the
canonical same-name DEntry from an atomic get-or-insert, and each bind mount
owns a distinct root DEntry projection over the shared source RNode. Mount flag
authorization is now closed for this loader slice: resolution/open retains the
final namespace-resolved Mount, main and interpreter reject `MS_NOEXEC` after
symlink or mount crossing, and set-id policy consumes `MS_NOSUID` from the final
alias mount.

核心 host verification：

- `cargo test -p tx-scripts process::exec::loader::tests --lib`：70 passed；
  当前 70 个永久用例包含一个 no-panic property corpus。该 corpus 使用固定
  seed 覆盖 2048 个确定性 fixture 变异与随机 bounded inputs，同时调用
  `Elf08Parser` header/program-header 解码和 image-plan 构建。
- 迁移期 differential test 使用精确的 `goblin 0.10.5` backend，对三组
  fixture 的全部映射 ELF header/program-header 字段逐项比较并通过；通过后
  已删除该测试、backend 和临时 dev-dependency。
- `cargo test -p tx-scripts staged_elf_read_ --lib`：6 passed。
- `cargo test -p tx-scripts dynamic_exec_layout_ --lib`：9 passed。
- `cargo test -p tx-scripts process::exec::script::tests --lib`：33 passed；
  main/interpreter 都经 process mount namespace 解析，且只使用声明的
  `PT_INTERP` 路径。
- `cargo test -p tx-shims linux_syscall::tests::execve --lib`：12 passed；
  `ShimsTestPmap` 现显式提供 `USER_TOP`，与生产 loader policy 契约一致。
- `cargo check -p tx-scripts --lib`：passed。
- `cargo tree -p tx-scripts -e normal`：包含 `elf v0.8.0`，不再包含
  `goblin`。
- `cargo xtask progress validate`：passed，34 个 progress records 合法。
- `cargo xtask lint docs`：passed，保留 7 个既有 stale-vocabulary warning。
- scoped `git diff --check`：passed。

完整 exec 计划仍保持 active。未完成项是 PageBacked-owned executable
lease/ETXTBSY 语义、可在 I/O wait 后恢复的 yielding `ExecScriptOp`、最终
GNU-stack permission matrix，以及 RV64/LA64 的 static、PIE、musl、glibc
guest witnesses。vDSO `__vdso_rt_sigreturn` 已落地，映射成功时 exec 会把
兼容 RWX 栈降为 RW；剩余缺口是尊重显式 `PT_GNU_STACK PF_X`，并验证无
vDSO restorer 时的 executable fallback。2026-07-15 closeout fresh checks:
`cargo test -p tx-scripts process::exec --lib` 147 passed，workspace
`cargo check` passed，normal dependency tree 只有 `elf v0.8.0` 且无
`goblin`。`cargo -q xtask unit` 中 `tx-shims` 599、`tx-ext4` 27、
`tx-scripts` 147 全部通过。2026-07-16 follow-up 以 TDD 重现并修复了
`tx-kernel` 的三个 bootstrap exec fixture：init `TestPlatform` 现在声明
`FULL_USER_V1_TOP`，与 shims 测试平台一致；保存的 SP 按实际 ASLR layout
验证为 replacement AddressSpace 内的 16-byte-aligned 映射。随后
`cargo -q xtask unit` 全绿：tx-shims 599、tx-kernel 107、tx-ext4 27、
tx-scripts 147。详见
`docs/progress/plans/2026-07-14-elf-exec-loader.json`。

2026-07-16 guest witness update：RV64 static ET_EXEC 已通过
`cargo xtask shell-test --target rv64-qemu --script
tools/shell-tests/busybox-prompt.txt`，日志为
`target/elf-witness/rv64-busybox-prompt-60s.log`。Alpine 的 dynamic PIE
BusyBox（`PT_INTERP=/lib/ld-musl-riscv64.so.1`）及 static-PIE musl loader
已通过 `alpine-vi-open-smoke.txt`，日志为
`target/elf-witness/rv64-alpine-vi-dynamic-musl-30s.log`；工件类型见
`target/elf-witness/elf-artifact-types.log`。其余 guest closure 仍未完成：
LA64 的 signal exit-group API 名称漂移已修复，LA64 tx-shims target check
已通过；尚未 fresh 重跑 guest build，当前仍缺 vendored LA64 BusyBox，且
共享 worktree 的 reactor 重组暂时不能编译。musl LTP `execve01,execve06`
因 `tx-test-init` 无法创建 `/usr`、
`/var`、`/lib`、`/tx-ltp` 而没有发出 `RUN LTP CASE`，judge 为 `0/0`；
备用 runtest 路径在有界窗口内只完成了 4 GiB image 的部分复制；glibc
LTP 被未运行的 Docker/Colima daemon 阻断。上述 blocked/`0/0` 结果均不
计为通过。

2026-07-16 exec-side follow-up：`ExecScriptOp` 已移除 one-shot/`EBUSY`
桥，PageBacked/VFS 的 `YieldShape` 由 `sys_execve` 交给中央 waiting driver
停放并在唤醒后重新执行 PoNR 前准备；临时 address space 和 credential
reservation 在停放前按 RAII 回滚。GNU-stack 权限矩阵也已闭合：显式
`PT_GNU_STACK PF_X` 保留 RWX；有 vDSO restorer 且无 PF_X 时为 RW；无
vDSO restorer 时保留 RWX signal-frame trampoline 回退。剩余核心缺口仅为
PageBacked/VFS 所有的 executable lease/ETXTBSY，完整计划仍等待 LA64 和
LTP/glibc guest witnesses。

2026-07-17 final ELF ABI review follow-up：生产 initial stack 现在在受检查的
stack string pool 中实际放置 NUL 结尾的 `AT_PLATFORM` 和 `AT_EXECFN`
字符串，并由实际地址生成 auxv 指针。`AT_PLATFORM` 严格由
`P::ARCH` 选择 `riscv64`/`loongarch64`；`AT_EXECFN` 保留用户最初传入的
`execve` pathname，shebang 和 ELF `PT_INTERP` 重写不改变它。栈大小、
地址、指针、`USER_TOP` 和 16-byte SP 对齐全部通过 fallible checked
运算验证。非 ELF 且无有效 `#!` 的文件回到 Linux `ENOEXEC`
语义，删除了内核 `/bin/sh` fallback；第 5 次 shebang 重定向返回
`ELOOP`。

同日 compatibility correction：shebang probe 对齐 Linux
`BINPRM_BUF_SIZE=256`，newline/NUL 正常终止，无终止符满窗口时只在
interpreter token 可能截断时拒绝，optional text 即使截断仍保留为一个
完整参数，并把 byte 255 留作强制 NUL。argv/envp 的内嵌 NUL 在
`exec_script` 和 stack builder 两层拒绝；空 pathname 直接返回 `ENOENT`；
syscall argv/envp 外层向量与 proc 引用向量使用 fallible reserve 并把故障
注入失败映射为 `ENOMEM`；字符串字节增长也走 fallible reserve，坏的
argv/envp 数组或元素指针保持 `EFAULT`，注入 guard 在 unwind 后自动复位。
`AT_PHDR` 不再采用上述 strict policy，而是按
Linux 从首个覆盖 `e_phoff` 的 `PT_LOAD` 计算 file-offset/VA delta；不要求
整个 phdr table 落在同一个 LOAD，且 `PT_PHDR` 缺失、重复或不一致不构成
额外拒绝；高 `e_phoff` 负测覆盖 phdr table end 跨 `USER_TOP`。host 验证为
exec 163/163、loader 73/73、script 63/63、shim execve 23/23；
`cargo -q xtask unit` 为 shims 636、kernel 113、ext4 31、scripts 163。
本更新没有新增 guest witness，既有 LA64/LTP/glibc blocker
不变。
