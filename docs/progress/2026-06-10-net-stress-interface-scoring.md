# net_stress.interface 官方计分清单与攻坚状态(2026-06-10)

判分机制(已逐环验证,详见 STATUS 2026-06-10 scoring addendum):官方
`ltp_testcode.sh` 遍历 `ltp/testcases/bin/*` **无参执行**每个文件;judge 只认
新框架的 `Summary: passed N` 块;case 名取 RUN 行末 token。因此本家族**只有
bin/ 里这些脚本文件的无参默认形态(CMD=ip、IPv4)计分**;runtest 清单里的
`-c ifconfig`/`-c route`/`-6` 变体不独立计分。

## 计分测试清单(bin/ 遍历会执行的本家族文件)

| bin/ 文件(官方无参形态) | 等价 runtest 变体 | 状态 | 预算 |
|---|---|---|---|
| `if4-addr-change.sh` | if4-addr-change_ifconfig | ✅ **PASS(+1)** | ~2min |
| `if-addr-adddel.sh` | if4-addr-adddel_ip | ✅ **PASS(+1)** | ~2min |
| `if-route-adddel.sh` | if4-route-adddel_ip | ✅ **PASS(+1)** | ~2min |
| `if-updown.sh` | if4-updown_ip | ❌ 预算墙 | 实测 ~1300s,LTP 限时 300s → **须砍 4.5×** |
| `if-addr-addlarge.sh` | if4-addr-addlarge_ip | ❌ 预算墙 | 实测 ~680s → **须砍 2.3×** |
| `if-route-addlarge.sh` | if4-route-addlarge_ip | ❌ 预算墙 | 实测 ~1200s → **须砍 4×** |
| `if-mtu-change.sh` | if4-mtu-change_ip | ❌ 预算墙(末位) | 自带 `tst_set_timeout` 3100s;纯 ping 间隔下限 2000s(100 轮×4 尺寸×500 发×10ms),当前 ~3600s+ → 须把 ping 轮均 <14ms 才有戏 |
| `if-lib.sh` / `tst_net_stress.sh` / `tst_net.sh` | (库文件被遍历) | ⛔ 结构性 0 分 | 无参执行 TST_TESTFUNC 未定义 → TBROK;不改测试文件不可得分,放弃 |

**当前到手:+3。可再争取:+3(updown/addr-addlarge/route-addlarge),mtu-change +1 为远期。**

## 预算墙成本分解(if4-updown_ip 实测)

检查周期(每 5 轮 down/up 一次)≈ **77s**,其中:
- ping 500 发 ≈ 9s(分配器修复后轮均 17.9ms,接近 10ms 间隔下限);
- 其余 ~68s ≈ 25-30 个命令调用 → **~2.5s/命令**(adddel 测得 ~0.75s/命令)。

待证嫌疑(按优先级):
1. **PATH 包装层**:`/tx-ltp/bin` 下的 CMDWRAP shell 包装脚本在**非 trace 模式
   也在 PATH 上**,每个命令多付一次 sh 解释器 exec(待核实 bin/ 与 trace-bin/
   的实际内容分布;若属实,把纯转发命令改为 busybox 符号链接,语义 shim
   保留脚本)。
2. **`ifconfig down/up` 内核侧级联**:updown 的 2.5s/命令显著高于 adddel 的
   0.75s,差异集中在 SIOCSIFFLAGS 路径(set_device_up_by_ifindex 之后的
   iface/路由重建成本待测)。
3. **tst_rhost_run / DAD 链**:每次 restore 一条 ns-exec 链(~5 exec)。
4. fork/exec 基础成本(ipneigh 台账测过 ~20-25ms/链,与现状差 1-2 个量级,
   提示 1/2 类结构性开销而非纯 TCG)。

## 验收口径

- 攻坚见证统一跑 **`_ip` 变体**(与官方无参形态同代码路径),普通
  run_bench_groups.sh(不带 timeout_mul)下 **PASS LTP CASE = 过线**
  (= LTP 自身 300s 限时内完成)。
- mtu-change 单独口径:tst_set_timeout 3100s 内完成即计分,但占用官方
  墙钟 ~40-50 分钟,最后再攻。
