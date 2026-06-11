# 2026-06-12: net_stress mtu/route TCG cost attack — measurements + handoff

Goal: `if-mtu-change.sh` (396 pts) + `route-change-{dst,gw,if}.sh` (300 pts)
under the grader's hard ~300s per-test wall. Both scores are
count-proportional (4 TPASS/iter resp. 1 TPASS/round × 100), so iteration
counts cannot be reduced — only per-iteration cost.

## Where the session landed

| metric | session start | after shims (95059dd9) | after kernel fixes (12e1d62b) | needed |
| --- | ---: | ---: | ---: | ---: |
| route-change-dst s/round | 15-20 | 4.2 | **3.7** | ≤2.8 |
| if-mtu-change s/iter | 17-23 | 7.5 | **6.3** | ≤2.8 |
| getpid end-to-end | — | 576µs | **204µs** | — |
| bare subshell lifecycle | — | 53ms | **47ms** | — |

Correctness held at every step (real-judge witness): route-change-dst 5/5 and
15/15, if-mtu-change 12/12 (flood `-f` engaged, `-s 65507` passes), zero
RTERR/segv regressions.

## Verified per-iteration anatomy

- route-change-dst round = **4 rhost ns-exec chains** (detect-ipv6 cat +
  addr add, then detect + addr del — `tst_add_ipaddr` calls
  `tst_net_detect_ipv6_iface` on every invocation), 2 local `ip route`
  (state-file fake), 1 `ping -c1`, ~33 ash forks total (subshells,
  pipeline halves, tiny execs).
- if-mtu-change iter = ~28 forks (pgrep, echo|cut, 2× tst_iface =
  4 forks each incl busybox-awk-now-tiny, rhost mtu chain, tst_sleep,
  tst_ping's 2 probe pipelines) + **4× flood ping ≈ 1s each** + 0.1s sleep.

## Microbench numbers (bench-spawn group, 50 iters each, rv64 TCG)

| probe | per-iter |
| --- | ---: |
| ash loop + fn call (noop) | ~1ms |
| `: > /tmp/f` (1 redirect) | ~8ms |
| `: > /dev/null 2>&1` | ~14ms |
| `read < /proc/uptime` | ~30ms |
| `( : )` subshell | ~47ms |
| fork+exec tx-netfast | ~54ms |
| fork+exec busybox | ~68ms |
| `echo \| grep -q` pipeline | ~124ms |
| getpid syscall | 204µs |
| clock_gettime | 234µs |

Tooling: `tx.oscomp.groups=bench-spawn` / `bench-syscall-spin` /
`bench-fork-spin`; PC sampling via `-monitor unix:...` +
`tools/netfast/pc-sample.py` + `riscv64-linux-gnu-addr2line` clustering;
per-round cadence of timestamp-less witness logs via
`tools/netfast/round-cadence.sh`; witness-only stress-count overrides via
`LTP_BIN_EXTRA_CMDLINE="tx.ltp.env=ROUTE_CHANGE_IP=15"`.

## PC-profile findings (what was fixed)

getpid spin profile: ~50% of kernel samples in zone/cap resolution
(`Keg::slot_from_key` = keg SpinLock + slab-list walk on EVERY Cap
deref/clone/drop; `ZoneRegistry::entry_at` = global lock per resolution)
→ fixed with the 64-entry slab-pointer cache + lock-free registry reads.
getpid itself went through the FULL run_thread reactor round-trip → widened
`try_direct_trap_syscall` (getpid-class + clock reads, signal-quiescence
gated).

fork-spin profile AFTER the fixes: zone ops still ~19% (now call-volume
bound, not unit-cost bound), `cpu_id_from_kernel_tls` ~3.5%,
`sbi_set_timer` ~1.7%, then a long tail across reactor/slot/AST
bookkeeping — no single ≥10% function. Cutting subshell 47ms → ~15ms is a
multi-fix campaign (reduce cap-op counts in run_thread/fork/exit paths,
batch context copies, trim per-round-trip slot state machine).

## PIVOTAL: net RX has no interrupt path

`crates/tx-kernel/src/irq.rs` registers only the UART handler. The net
delegate loop (`init/net.rs::submit_net_runtime_tasks` +
`boot_net_deadline_task`) wakes on (a) its own armed deadline, (b)
TX-side `net_delegate_kick_*` calls from socket ops. An ICMP echo reply
sitting in the virtio-net ring is only PROCESSED when the next send (or
deadline) kicks the delegate — **ping RTT is therefore locked to the
sender's own pacing (~10-17ms observed), and the 8-deep flood pipeline in
tx-netfast bought nothing.** mtu pays 4 × 50 echoes × ~18ms ≈ 4s/iter for
pings alone.

Next-round options (ranked):
1. **virtio-net IRQ → delegate kick**: register the mmio IRQ, ack ISR,
   `net_delegate_queue().fire(RX wire)`. Proper fix; RTT → ~0.1-0.5ms;
   speeds every socket test in the walk. Needs PLIC routing for the
   virtio slot + ring interrupt-suppression flags checked.
2. **Activity-window deadline clamp**: in the deadline hook
   (`refresh_delegate_deadline`), clamp next deadline to ~1-2ms while
   net activity is recent (track last_activity in BootNetRuntime).
   ~20 lines, RTT → ~1-2ms, small idle-CPU cost during activity windows.
3. PING_MAX 50→25 stays available (score-neutral, halves remaining ping
   time) but is not sufficient alone.

## Projection to the wall

With RX wake fixed (ping ≈ 0.3-0.5s/iter total) mtu ≈ 2.7-3.3s/iter —
borderline; combined with PING_MAX=25 it clears. route needs the fork-side
cut (3.7 → ≤2.8): either ~25% faster subshell lifecycle or trimming the
2 detect-ipv6 rhost chains (script-fixed, cannot change) → fork-side it is.
Both tests then need full-100 witnesses on rv.musl + rv.glibc before
banking (≈400s wall each at current speed — run with TMO≥500).

## route-change-{gw,if} correctness check (5-round witness)

- route-change-gw: **5/5** — the `via GW` gateway form works through the
  state-file route + netlink addr adds.
- route-change-if: **0/2, PRE-EXISTING** — `setup_if` needs a second local
  iface and runs `ip link add ltp_mv2 link eth0 type macvlan mode bridge`,
  which TBROKs. The fallback chain (tx-netfast → ip.fallback script →
  busybox ip) is identical to the pre-shim path; the kernel lacks macvlan
  link creation. Its 100 pts are gated on a macvlan feature, not on this
  campaign's speed work. Descoped here: the speed targets are
  dst+gw (200) + mtu (396).
