# VM pmap 全路径性能审计

日期：2026-08-01

## 结论摘要

当前最强的实测热点不是单独的 `rfence`，而是 `VmPmap::state` 自旋锁覆盖了
硬件页表修改、shootdown、resident shadow store 更新和 `MapPin` 生命周期。连续
teardown 的第二个结构热点是 Vec resident store 的 suffix 搬移；所有三种平台还
在每个 4 KiB unmap 后逐级扫描页表判断中间节点是否为空。现有“batch”并非一个
端到端批处理接口：substrate batch 主要延迟 pin release，HAL 默认实现仍逐项
调用 singleton，RV64 的远端 SBI 也对每个合并区间分别发起 RFENCE。

因此，下一步的正确顺序是：先把 shadow/index、PTE mutation、invalidation
collection、remote RFENCE 四层边界和计数器补齐，再分别实现 protect/mutation
batch；chunked resident 要经过保留同一 workload 的 guest A/B 后才能切默认。

## 问题清单

| 优先级 | 问题 | 代码证据 | 证据等级 | 影响与建议 |
|---|---|---|---|---|
| P0 | `pmap.state` 锁持有过重 | `crates/tx-subsystems/src/vm/pmap.rs:249-299`；`308-367` | 已测 + 代码确认 | publish replacement 在锁内执行 HAL unmap、singleton shootdown、pin drop、reserve/commit 和 resident insert；prefault batch 在同一锁内逐页 reserve/commit/insert。历史锁测量约 320408 次、7.319s，p99 169us、max 588us。应将硬件 mutation/shootdown 与 shadow 更新拆成可验证的阶段，至少禁止在 shadow 锁内做整批硬件工作。 |
| P0 | protect 仍逐页 shootdown | `crates/tx-subsystems/src/vm/pmap.rs:468-510` | 代码确认；RV64 行为可推导 | 每个 resident page 都调用 `shootdown_mappings(asid, &[one])` 并递增计数，破坏相邻区间合并并重复远端协调。改为收集 ordered invalidations，成功路径一次 issue，错误路径 flush 已修改前缀。 |
| P0 | 平台 root destroy/ASID reuse 仍是批量 mutation 的正确性门禁 | `boards/tx-hal-riscv64-qemu-virt/src/pmap/address_space.rs:103-113,465-472`；`docs/progress/STATUS.md:13883-13903` | 已有 SMP blocker | destroy 只做本地 `sfence.vma`，未证明当前/远端 hart 已切离 doomed root 就清 residency 并复用 ASID。任何扩大 mutation batch 或延迟释放 pin 的实现都必须先保留该门禁。 |
| P1 | resident Vec 的 drain/insert 搬移 | `crates/tx-subsystems/src/vm/pmap/resident.rs:141-179`；`pmap.rs:391-407,430-446` | 已测 | sorted Vec 的中间 drain/insert 移动 suffix；replacement 可能 remove+insert 两次。map-path 基线：teardown 总计 9.693s，drain 2.382s，shifted entries 最大 11295，单次 drain 最大 27.810ms。chunked 已实现但默认仍是 Vec，需同 workload lossless A/B。 |
| P1 | 每页 unmap 都做 PT-node 空表扫描 | RV64 `boards/tx-hal-riscv64-qemu-virt/src/pmap/address_space.rs:327-346,559-600`；LA64 `boards/tx-hal-loongarch64-qemu-virt/src/la64_pmap.rs:601-625,835-903` | 跨平台代码确认 | 4 KiB unmap 后逐页检查完整页表（RV64 每级最多 512 entries，LA64 更多级），连续 teardown 形成 `O(pages * entries * levels)` 放大。优先增加 touched PT page occupancy counter，或把 prune 延迟到 range mutation batch 末尾。 |
| P1 | teardown 的临时 Vec 和 HAL loop 重复分配/遍历 | `crates/tx-subsystems/src/vm/pmap.rs:414-452`；RV64 coalescer `boards/tx-hal-riscv64-qemu-virt/src/pmap/address_space.rs:408-417` | 代码确认 | 已知 `drained.len()` 却从空 Vec 开始收集 invalidations/pins，RV64 又分配等容量 coalesced Vec。改为 `with_capacity(drained.len())`，并让 batch 直接复用固定/可增长缓冲；同时将 unmap PTE mutation 和 invalidation collection 合并为一遍。 |
| P1 | “batch”接口分裂，实际不保证一次底层 flush | `crates/tx-substrate/src/lib.rs:331-445`；`crates/tx-hal/src/lib.rs:822-827`；RV64 `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:1757-1778` | 代码确认 | `AddressSpaceShootdownBatch` 的核心语义是 shootdown 前保留 pin；HAL 默认 plural fallback 逐项 singleton；RV64 只在本地 coalesce 后对每段分别发 SBI RFENCE。需要明确四层 API：shadow/index batch、PTE mutation batch、invalidation batch、remote transport batch。 |
| P1 | fork CoW 保护存在无效工作，child 为空导致 refault | `crates/tx-subsystems/src/vm/execution.rs:111-140` | 代码确认 + 历史设计 | 对不可写 recipe 也会枚举 resident 并分配 page vector；child pmap 当前不复制父热页，后续访问重新 fault。先跳过无需降权的 VMA，再决定是否做受控的 parent resident RO 共享，不能把两者混为 shootdown 优化。 |
| P1 | mprotect/mremap 仍是 teardown + refault | `execution.rs:1001-1049,1173-1192`；`docs/design/03_memory-vm/VM_v1_2.md:651-705,1103-1112` | 设计明确 deferred | mprotect 丢弃现有 PTE/MapPin，后续重新 fault；mremap move 可能 old/new 两次 teardown/shootdown。in-place permission patching 是 deferred，只有在 mutation batch 和并发语义明确后再做。 |
| P2 | mincore 重复 pmap walk 和逐字节 user write | `crates/tx-shims/src/linux_syscall/vm.rs:781-795`；`crates/tx-subsystems/src/vm/execution.rs:1253-1265` | 代码确认 | 先逐页 recipe lookup，再 `mincore` walk 一遍 pmap，最后每个结果字节调用一次 `bootstrap_write_user`。应提供 range snapshot/bitmap 输出并一次性 copyout。 |
| P2 | Direct I/O 对同一页集合多次遍历并分配 | `crates/tx-shims/src/linux_syscall/io.rs:1699-1721`；`crates/tx-subsystems/src/page_backed/direct_io.rs:194-231` | 代码确认 | 先 `ReserveUserRangeOp`，再 `DirectIoBuffer::pin` walk snapshots，随后分别生成 DMA pins 和 BioVecs。可让 reserve 返回可复用 snapshot/lease，避免二次 walk；BioVec/pin 仍需保持所有权和异步 lifetime。 |
| P2 | user copy 的预留和实际 copy 重复 resolve | `crates/tx-subsystems/src/vm/user_access.rs:203-255,345-440,489-520` | 代码确认 | range reserve 逐页 lookup/publish，随后 copy in/out 每页再次 `resolve_user_page_addr`/lookup。可在同一 syscall step 中复用已验证 page lease；不能跨 yield 持有普通 epoch guard。 |
| P2 | 观测参数在 cfg 关闭时仍先求值 | `crates/tx-subsystems/src/vm/execution.rs:374-382`；`1117-1124`；`pmap.rs:206-214` | Rust 求值语义确认，成本未单独测 | `emit_vm_trace` 的参数先求值，因此 metrics 关闭时 `pmap.stats()` 仍获取 state 锁。将昂贵表达式放入 cfg 宏/闭包后再用 profile 验证收益。 |
| P2 | registry/ASID 分配在 churn 下可能退化 | RV64 PT-node registry 与 ASID allocator；LA64 固定 4096 registry 扫描 | 未测推断 | 全局 spinlock/tombstone、bitmap 从头扫描、每次 activation 的 residency 原子更新会放大高 churn/SMP 流量。先补 counters 和压力测试，再决定 compaction/next-hint/generation 策略。 |

## 为什么已有 shootdown batch 仍没有解决 protect

当前有三种容易混淆的 batch：

1. `PmapIf::shootdown_mappings` 是 HAL 的 plural 接口；默认实现逐项调用
   `shootdown_mapping`，只有 RV64 QEMU 做相邻 invalidation coalesce。
2. `AddressSpaceShootdownBatch` 是 substrate 的 pin-release 生命周期容器；它
   在 `issue_and_release` 中把切片交给 `PmapIf::shootdown_mappings`，并不定义底层
   fence/IPI/SBI 的传输批量。
3. `tx_hal::pmap` 的 range helper 只是逐页 reserve/commit/unmap/protect 的便捷
   外壳，`VmPmapOps` facade 和生产 VM 路径没有接入。

`teardown_range` 在 5 月 31 日已经接入自己的 invalidation collector，但
`protect_range` 是 5 月 13 日为 fork CoW 引入的窄路径；后续只增加了 resident
page 枚举，没有把 protect 迁移到 collector。因此这是历史接线遗漏，不是没有
shootdown batch 能力。

## 建议的实施/验证顺序

1. 为 `protect_range` 加多页成功和部分失败测试，断言一次 batch、ordered
   invalidations，以及错误前缀已 flush。
2. 把 `VmPmap` 的 batch 状态拆成四层，先让 protect 和 teardown 共用
   invalidation collector；容量已知时使用 `with_capacity`，必要时固定栈批量。
3. 在各平台分别实现 range PTE mutation/prune；RV64 继续 coalesce，但同时
   评估 SBI transport 是否支持真正 range/全 ASID 阈值切换；LA64/M1Dock 不能
   继续静默 fallback 为逐页而不计数。
4. 补充 `pmap.state` lock wait/hold、PTE mutation、local fence、remote IPI/SBI、
   PT prune、resident shifted entries 的独立 counters。
5. 在同一 guest workload 上做 `Vec`/`Chunked` A/B，比较
   `teardown_total/drain/shift`、protect batch count、fork/clone latency 和
   sparse insert latency；没有 lossless receipt 不切默认。
6. 先处理 RV64 root/ASID/SMP 和 LA64 shootdown/activation correctness 门禁，
   再扩大 pin 延迟释放或跨 hart 的 mutation batch。

## 验证状态

- 已完成 CodeGraph 调用图、源码行号、git history/blame 和现有观测 artifact
  审计。
- 复核了 `publish_page_with_replacement`、prefault publish、teardown、protect、
  resident store、RV64/LA64 pmap、substrate batch、mincore、Direct I/O 和 user
  copy 路径。
- 本轮未修改 Rust，未运行新的 benchmark 或 guest；性能数字均来自已有
  `docs/progress/research/2026-06-04-mmap-munmap-map-path.md` 和锁测量记录。
- 下一步实现应使用 TDD，先建立 protect collector 回归，再做平台逐项验证。
