# net_stress.ipsec 家族评分分析 — 结构性 0 分，建议不立项 (2026-06-10)

## 结论

官方评分机制下（`ltp_testcode.sh` 遍历 `bin/*` 无参执行 + judge 只数
`Summary: passed N`），**9 个 ipsec 脚本全部是结构性 0 分 —— 即使换成真
Linux 内核也是 0 分**。不需要 xfrm、不需要加密算法、不需要 vti。除非
官方镜像/judge 改版，否则这个家族不值得投入任何内核工作。

## 镜像内的 ipsec 计分候选（bin/ 实存文件，sdcard-la.img 抽取验证）

| 脚本 | 无参行为 | passed |
|---|---|---|
| tcp_ipsec.sh | 见"非 vti 路径" | 0 |
| udp_ipsec.sh (TST_CNT=2: udp, udp_lite) | 同上 | 0 |
| dccp_ipsec.sh | 同上（server 先因无 DCCP TCONF） | 0 |
| sctp_ipsec.sh | 同上 | 0 |
| icmp-uni-vti.sh | 见"vti 路径" | 0 |
| tcp/udp/dccp/sctp_ipsec_vti.sh | 同上 | 0 |
| ipsec_lib.sh（被直接执行） | `grep -q tst_run` 失败 → TBROK | 0 |
| output_ipsec_conf（数据文件被执行） | 噪音，无 Summary | 0 |

## 证据链

1. **无参时根本不配 IPsec**：`tst_ipsec_setup` 只在 `-m`（IPSEC_MODE）和
   `-p`（IPSEC_PROTO）同时给出时才下发 SAD/SPD（ipsec_lib.sh:108）。
   runtest 清单里的 `-p esp -m transport ...` 参数官方从不传。
2. **非 vti 路径仍然必挂**：`do_test` 拼 `local opts="-n $2 -N $2"`，无参时
   `$2=""`（tst_test.sh `_tst_run_test "$TST_TESTFUNC" $i ""`），未加引号展开成
   `-n -N` 两个 token；tst_netload 的 getopts 把 `-N` 当作 `-n` 的 OPTARG 传给
   netstress 客户端；netstress（LTP 20240930）`tst_parse_int("-N", min=5,
   max=65535)` 失败 → `tst_brk TBROK "Invalid client msg size '-N'"` → 退出码 2
   → tst_netload `ret&3 != 0 && was_failure` → TFAIL → passed=0。
   这在真 Linux 上同样发生（与内核能力无关）。
3. **vti 路径无参必 TCONF**：`tst_ipsec_setup_vti` 先
   `tst_check_drivers ip_vti`（我们没有 → TCONF），即使有，
   `ipsec_set_algoline` 在 `IPSEC_PROTO` 为空时走 `*)` 分支
   `tst_brk TCONF "tst_ipsec protocol mismatch"`（ipsec_lib.sh:161）。
   真 Linux 同样 TCONF → passed=0。
4. **judge 不罚 failed/broken**：judge_ltp-musl.py `score = Summary passed`，
   所以这些脚本跑挂也不丢别处的分；唯一风险是挂死占预算 —— 现有 setsid
   reaper（net_stress.interface 战役产物）已兜底。

## 如果将来仍要做真 IPsec（非计分动机：runtest 形态、答辩展示）

需要的全套（按依赖序）：
- NETLINK_XFRM 真实语义：XFRM_MSG_NEWSA/NEWPOLICY/DELSA/…（现状
  `nfnetlink.rs:357` 是 stub：flush ACK、get/dump 空 DONE、其余 EOPNOTSUPP —
  正好够 ipsec_lib.sh 的 cleanup 路径不报错）。
- IP 收发路径挂 SPD lookup + ESP/AH/IPComp transform，transport/tunnel 两模式。
- vti/vti6 link type（RTM_NEWLINK + ikey/okey/mark 路由）+
  tst_check_drivers 可见性（modules.builtin / /proc/modules）。
- 加密算法**应当用第三方 no_std crate，不要手写**：RustCrypto 系列
  （aes、des [des3_ede]、cbc、hmac、sha1/sha2、md-5、aes-gcm [rfc4106]），
  IPComp deflate 用 miniz_oxide。均为 MIT/Apache、no_std。
- 现有底子：external/smoltcp-asterinas 已带 wire/ipsec_esp.rs +
  wire/ipsec_ah.rs（仅 wire 层解析，无 SA 管理/加解密）。

## 验证方式

全部结论来自官方镜像一手抽取（debugfs -c dump：ipsec_lib.sh、
tcp/udp/dccp/sctp_ipsec[_vti].sh、icmp-uni-vti.sh、tst_net.sh、tst_test.sh、
ltp_testcode.sh、judge_ltp-musl.py）+ LTP 20240930 netstress.c 上游源码核对。
未跑 QEMU（不需要：失败点在脚本/judge 层，与内核无关）。
