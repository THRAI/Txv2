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

---

## 2026-06-12 (later session) — RX-WAKE ROOT CAUSE REFUTED BY MEASUREMENT

**The "net RX has no interrupt path → ping RTT locked" premise above is WRONG
for net_stress.** Implemented option B (activity-window deadline clamp) two
ways — first keyed on virtio device TX/RX, then broadened to *any* delegate
step that moved traffic (new `net_delegate_last_activity_micros()` global,
stamped in `net_delegate_step_once` when `moved_traffic()`), plus a
`net_delegate_kick_poll()` on every deadline-timer wake. Built clean, staged,
witnessed. **Both versions left if-mtu-change at ~7s/iter — no change.** Then
instrumented with three exit-dumped counters (clamp engagements, deadline
polls, delegate-hook calls + last_activity):

| test (rv.musl witness, exit dump) | clamp | dl_polls | dl_calls | last_act |
| --- | ---: | ---: | ---: | ---: |
| if-mtu-change (MTU_CHANGE_TIMES=3) | 0 | 0 | 6 | 0 |
| route-change-dst (ROUTE_CHANGE_IP=5) | 0 | 0 | **0** | 0 |

**The boot net delegate is DORMANT during net_stress.** `dl_calls=0` for
route-dst means its task loop ran zero steps the entire test; `last_act=0`
means no delegate step ever moved traffic. So the virtio ring + delegate path
the RX-wake fix targets is **not on the critical path for these tests.**

**Why:** `step_send_raw_icmp` (`net/execution/step_send.rs:~530-604`)
SYNTHESIZES the ICMP echo reply in-line and delivers it straight to the socket
table via `deliver_icmpv4_reply_to_table` — the kernel answers the ping inside
the sender's `sendto` syscall. No virtio TX, no veth `transmit`, no delegate,
no RX wake. (veth `transmit` does sync-push to the peer rx queue, but the LTP
pings are SOCK_RAW ICMP and never reach it — the echo responder short-circuits
first.) Both gateway pings (route-dst) and netns/veth pings (mtu) take this
synchronous path.

**Corrected cost model (measured via score-neutral PING_MAX sweep):**
if-mtu-change per-iter ≈ **7s @ PING_MAX=50, ≈5s @ PING_MAX=5**. So the ping
component is only ~1.5-2s/iter (4 tst_ping invocations × PING_MAX packets ×
~per-syscall TCG cost — NOT RX latency, since the reply is synchronous) and
the **irreducible floor is ~5s/iter of fork + ns-exec** (≈28 forks + 4
tst_rhost_run setns chains). That floor alone is >> the ≤2.8s wall.

**Conclusion: neither option A (virtio IRQ) nor option B (delegate clamp) can
bring mtu/route under the wall.** Both target a dormant path. The earlier
"~10-17ms RTT locked to sender pacing" was a mis-measurement / mis-attribution
(likely conflated with a genuinely virtio-bound path like a real-internet
gateway probe, which net_stress does not exercise). All option-B + diagnostic
code was REVERTED (working tree clean at HEAD ae8fffd9); the deliverable is
this measurement.

**Real levers for mtu/route (unchanged from 2026-06-11), all fork/ns-exec/
syscall-cost bound under TCG:**
1. Make `tst_rhost_run` ns-exec (setns + fork + exec `sh -c`) cheaper — 4
   chains/iter dominate. The kernel `setns`/spawn path under TCG, not net.
2. Cut subshell/fork lifecycle (47ms) — reactor round-trip + cap-op volume +
   `cpu_id` TLS + `sbi_set_timer`; the long-tail campaign from the zone-cache
   session. This is a general spawn-cost problem, rippling outside net.
3. PING_MAX is the only ping lever and is already banked at 50 (score-neutral);
   lowering it further trims the ~1.5-2s ping slice but cannot touch the ~5s
   fork floor. Not sufficient.

Net delegate / virtio RX wake is a dead end for this scoring tier. If a future
workload IS virtio-RX-bound (TCP throughput to the QEMU user-net gateway, real
DNS/HTTP), the option-B clamp or option-A IRQ would help *that* — but no
scored net_stress test is.

## 2026-06-12 (later session) — ns-exec chain cost DECOMPOSED

Followed up the "make `tst_rhost_run` ns-exec cheaper" lever by measuring one
chain end-to-end. Extended the bench harness: netfast `bench-syscall` now also
times `openat(/proc/self/ns/net)` + `setns` (10k-iter loops); `bench-spawn`
now has `nsexec-{prog,cat,sub}` (50-iter, full chain via the real
`tst_ns_exec` binary, setns-to-`$$` — same-ns succeeds and takes the same
atomic-swap path as cross-ns, confirmed by reading `sys_setns` →
`replace_net_namespace` → `AtomicSlot::swap`). Kept (inert diagnostics).

**Per-iter wall (rv64 TCG):**

| shape | per-iter | what it is |
| --- | ---: | --- |
| subshell `( : )` | ~43ms | fork-only baseline |
| tiny-exec (tx-netfast) | ~51ms | fork+exec+wait baseline |
| bb-exec (busybox true) | ~70ms | fork+exec(big binary) |
| **nsexec-sub** = `$(tst_ns_exec $$ net sh -c "cat … \|\| echo RTERR")` | **~76ms** | the REAL tst_rhost_run shape |
| nsexec-cat (same, no `$()` wrap) | ~68ms | chain w/o the subst subshell |
| nsexec-prog = `tst_ns_exec $$ net busybox true` | ~88ms | direct-program form (2nd exec) |

**Per-syscall (10k-iter):** `openat(/proc/self/ns/net)+close` = **4.47ms** ·
`setns(fd)` = **0.68ms** · getpid 0.20ms · clock_gettime 0.22ms.

**Decomposition of the ~76ms real chain:**
- **fork + exec(tst_ns_exec) ≈ 51ms (≈67%)** — the bare spawn lifecycle,
  identical to tiny-exec. Net-external; this is the general TCG fork/exec cost.
- **openat ns-file ≈ 4.5ms (≈6%)** — procfs RNode materialization per chain;
  the *only* ns-specific cost of any size, but 6.5× the setns it feeds.
- **setns ≈ 0.68ms (≈1%)** — cheap `AtomicSlot::swap` of the netns cap; the
  nsproxy is NOT rebuilt. NOT a lever.
- in-proc `cat` + close + exit + `$()` read ≈ 20ms (≈26%) — more fork/syscall.

**Conclusion — the "ns-exec lever" collapses into the general spawn-cost
campaign.** There is no ns-specific bottleneck: setns is 0.68ms, and even the
4.5ms openat-ns × 4 chains/round = ~18ms is noise against route's 3.7s/round.
A route round is ~4 ns-exec chains (≈0.3s) **plus ~33 general ash forks**
(subshells, pipeline halves, tiny execs ≈ 43-121ms each ≈ 2.5-3s) — the ash
forks dominate, and they bottleneck on the *same* ~43-70ms/spawn TCG lifecycle
as the chains. So "speed up tst_rhost_run" ≡ "speed up fork/exec/subshell
under TCG" (CoW/vfork, exec demand-paging, reactor round-trip + cap-op volume +
sbi_set_timer) — net-external, high-risk, the long-tail campaign flagged
2026-06-11. The netns/setns path is NOT worth touching for this tier.

Bench-spawn baselines unchanged from earlier this session (getpid 198µs,
subshell 43ms, tiny-exec 51ms) → no regression from the bench-harness edits.

## 2026-06-12 (later session) — fork lifecycle PC profile → NO low-risk win

Since the ns-exec chain collapses into the general fork/exec cost, profiled the
bare subshell lifecycle to see if a low-risk local fix exists.
`tx.oscomp.groups=bench-fork-spin` (`while :; do ( : ); done`) + QEMU-monitor PC
sampler (1500 samples @4ms), addr2line-clustered against the high-VMA kernel
ELF (symbols at `0xffffffff80…`, sample directly — no offset). 1341/1500 (89%)
in kernel, 159 (11%) user (the ash loop).

**Top kernel buckets (% of all 1500 samples):**

| bucket | ~% | notes |
| --- | ---: | --- |
| **zone / cap / slab / lock machinery** | **~27-30%** | slot_for, Keg/Zone::slot_from_key, Cap clone/deref/drop, ZoneSlab, Slot, align_up, SlotKey, atomic_load, Option::map/branch (zone slot plumbing), SpinMutex lock+guard-drop, AtomicBool cas/store |
| cpu_id_from_kernel_tls | 3.6% | top single leaf; division-by-stride index calc |
| memcpy family (copy_forward + memcpy) | 3.6% | context/frame/page copies |
| sbi_set_timer | 1.7% | per-round timer reprogram |
| thread_future::run_thread | 1.5% | reactor scheduling round |
| page_allocator::return_to_free_pool | 0.7% | page free at exit |

**Conclusion — no low-risk lever clears the wall.** The cost is genuinely
diffuse: the single hottest leaf is 3.6%, and the dominant ~30% is zone/cap
*call volume* (fork clones the child's cap tables, exit drops them — the prior
O(1) slab cache already cut the per-op UNIT cost, so this is now count-bound).
Math: the cleanest micro-opts stack to ~5% of the 43ms fork (cpu_id ~1.5ms +
sbi_set_timer + memcpy) → mtu 5s→~4.75s/iter, nowhere near 2.8s. Even halving
the entire ~30% zone/cap bucket (invasive fork-resource-duplication surgery)
only reaches ~4.25s/iter. **Clearing the ≤2.8s wall requires ~HALVING the whole
general fork/exec lifecycle (43→~21ms)** — a multi-front, high-risk campaign
across every process-creation path (CoW/vfork + zone-op-count reduction +
reactor round-trip trim + cpu_id/timer leaves), all rippling outside net.

**Decision point reached:** the net_stress mtu/route tier is fork/exec-bound
with NO net-local and NO low-risk path. Options are (a) commit to the broad
fork/exec campaign (high risk, touches all spawn), or (b) pivot off mtu/route
to other bankable net tests. Surfaced to the user — fork surgery is the
explicit "ripples outside networking" stop condition for autonomous net work.
