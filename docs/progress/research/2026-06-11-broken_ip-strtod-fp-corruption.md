# broken_ip sender hang — REAL root cause: strtod returns 0.0 (FP corruption under heavy demand-paging)

Date: 2026-06-11
Lane: rv64 (musl); presumed same on rv.glibc / la.
Status: **root cause localized, NOT yet fixed.** The previous handoff's two
hypotheses (busybox `-t` arithmetic; kernel CLOCK_REALTIME frozen) are both
**REFUTED** below.

## TL;DR

`broken_ip-version.sh` (and the whole `broken_ip-*` family) hangs because the
sender `ns-icmpv4_sender`'s `send_packets()` loop never exits:

```c
for (;;) {
    sendto(...);                 // AF_PACKET, returns immediately
    if (fake_p->timeout)         // <-- this guard is FALSE every iteration
        if (fake_p->timeout < difftime(time(NULL), start_time)) break;
    if (catch_sighup) break;
}
```

`fake_p->timeout` is a `double` set by `strtod(optarg)` for `-t 2`. **At runtime
`fake_p->timeout == 0.0`**, so the guard is false, `time(NULL)` is never called
in the loop, and the loop spins on `sendto` forever. (Confirmed two ways: the
syscall trace shows 591×`sendto` and exactly **one** `clock_gettime` — the
`start_time = time(NULL)` before the loop, never again; and a kernel-side dump
of the `fake_p` struct from user memory shows `timeout` bytes all-zero.)

The argv is correct (`-t 2`), the kernel clock is fine (advances), getopt and
every integer field parse correctly. **Only the `double` from `strtod` is
wrong** — `strtod("2")` returns `0.0` instead of `2.0`.

## What it is NOT (handoff theories refuted)

- **NOT busybox `-t` arithmetic.** Kernel execve-argv dump shows the sender is
  invoked with literal `-t 2`. `tst_net.sh`'s `timeout=$(($NS_DURATION/$num))`
  = `$((10/5))` = 2, correct.
- **NOT a frozen CLOCK_REALTIME / vDSO.** (a) The vDSO is not even advertised:
  the live exec path (`tx-scripts/.../exec/script.rs::make_initial_user_trap_context`
  and the auxv builder) sets `at_sysinfo_ehdr: None`, so libc uses the
  `clock_gettime` *syscall*, which returns advancing realtime
  (`P::read_ns()+offset`). (b) A kernel heartbeat printing `realtime_now_ns()`
  while the sender spun showed realtime advancing normally. The handoff's
  `scounteren.TM` / `vdso.S` verification was of an **unused** path.
- **NOT preemption in isolation, netns, or static/dynamic linking.** A
  standalone `strtod("2")` test (both `-static` and dynamic against the image's
  own `/lib/ld-musl-riscv64.so.1`) returns 2.0; 300 000 calls in a loop: zero
  corruption; `unshare(CLONE_NEWNET)` then strtod: 2.0. Long-double / quad
  soft-float (`strtold`, `__multf3`, `__trunctfdf2`) all correct in isolation.

## What it IS

`strtod` is corrupted **only in the real sender's execution**, which differs
from every standalone repro by **page-fault volume**: the dynamically-linked
image binary demand-faults a flurry of cold libc *and* main-binary code pages
*during* strtod's long-double soft-float (frrm + many integer ops). The FP
trace during the sender's strtod shows ~10 instruction-fetch page faults plus
several timer interrupts, **all at FS=Dirty**, interleaved, in the few hundred
instructions of `__floatscan`.

Decisive isolation experiment (the key result):
- Recompiled `ns-icmpv4_sender` from LTP source **with my toolchain**
  (riscv64-linux-musl-gcc, dynamic, identical strtod→store instruction
  sequence — verified byte-identical: `jal strtod@plt; fmv.d.x fa5,zero;
  flt.d a5,fa0,fa5; bnez; fsd fa0,1568(s0)`), dropped it into the image, ran
  the **real broken_ip-version.sh** in its netns: `MYSND-TIMEOUT=2.0`, kernel
  dump `timeout=2.0`, and **`TPASS`**. Same context, same libc, **different
  binary → works**. The image binary (GCC 11.2.1, 58 KB, more code pages →
  more faults) fails; my binary (23 KB, fewer faults) passes.

So the corruption is **load-dependent**, triggered by the image binary's
heavier cold-page fault pattern during the soft-float.

Register preservation is **not** the cause: the trap FP save/restore (asm,
FS-gated), `capture_user_context`/`restore_user_context`, and the
`saved_user_context → enter_userspace_with_context` re-entry all correctly
preserve f0–f31 + fcsr + all x-regs across these FS=Dirty faults (verified by
reading every path and by register dumps across faults). The result is a
**clean 0.0**, which is consistent with strtod reading corrupted rodata
constants or executing a mis-mapped code page — i.e. a **VM / demand-paging
correctness issue under heavy concurrent instruction-fetch faulting + timer
reschedules**, not the FP context machinery.

This is the same *class* as the repo's known under-load VM bugs (see
`memory/page-allocator-contiguous-scan-rootcause.md`,
`memory/route4-livelock-pagetable-uaf.md` — pmap root reclaim/activate ordering
and allocator scan). Demand-paging the dynamic libc under the async page-fault
reschedule path is the suspect surface.

## Reproduction harness (for the next person — reusable)

The official image (`target/oscomp/testdata/sdcard-rv.img`) uses ext4
`metadata_csum`, which `debugfs`/`e2fsck` reject. To inject test binaries:

```sh
cp target/oscomp/testdata/sdcard-rv.img /tmp/sd.img
tune2fs -O ^metadata_csum /tmp/sd.img
e2fsck -fy /tmp/sd.img                       # rc=1 = fixed, OK
# LTP bins live at /musl/ltp/testcases/bin (runtime-mapped to /musl/musl/ltp/...)
debugfs -w -R "rm  /musl/ltp/testcases/bin/fptest" /tmp/sd.img
debugfs -w -R "write /tmp/mybin /musl/ltp/testcases/bin/fptest" /tmp/sd.img
# run our kernel directly (mirror tools/ltp-bin-witness.sh's QEMU line):
qemu-system-riscv64 -machine virt -kernel target/oscomp/submit/kernel-rv \
  -m 1G -nographic -smp 1 -bios default \
  -drive file=/tmp/sd.img,if=none,format=raw,id=x0,file.locking=off \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 -no-reboot \
  -device virtio-net-device,netdev=net -netdev user,id=net -rtc base=utc \
  -append "tx.oscomp.groups=ltp-bin:musl:fptest"
```
- `riscv64-linux-musl-gcc` is at `/usr/local/bin`.
- LTP `fptest01`/`fptest02` (already in the image) FAIL on rv64 — that is the
  **expected** x86-80bit-vs-riscv-128bit `long double` width difference, NOT
  our bug; do not chase them.
- Patched-sender source + build: `ns-icmpv4_sender.c` + `ns-common.c` (needs
  `ns-traffic.h`, `ns-mcast.h`) from
  `/home/msp/learning/new_os/ltp/testcases/network/stress/ns-tools/`, compile
  `-O2 -mabi=lp64d -march=rv64imafdc`, add a `write(2,...)` of
  `fake_data.timeout` bits right after `parse_options(...)`.

## Probe technique that found it

All in `tx-shims/src/linux_syscall` (execve in `proc.rs`, syscall heartbeat in
`mod.rs::dispatch`) + `boards/tx-hal-riscv64-qemu-virt/src/trap.rs`
(`dispatch_trap_frame`), gated by a flag set when a program path ends with
`icmpv4_sender`:
- execve argv dump + `fake_p` user-memory dump (via `bootstrap_copy_from_user`
  at the first AF_PACKET `sendto`, `args[4] = &daddr_ll`, `timeout` at
  `daddr_ll+36`).
- per-syscall heartbeat (nr + kernel realtime) — showed the loop is pure
  `sendto`, no in-loop `clock_gettime`.
- trap-class trace (ILL/TMR/PF) with `fs`, `sepc`, `stval`, fcsr, fa0/fa5/ft0,
  a0/a1/ra — showed FS=Dirty instruction-fetch faults flooding strtod.

(All probes were reverted; working tree is clean.)

## Recommended next steps (priority order)

1. **Chase the VM/demand-paging corruption under load.** Re-add the trap-class
   PF trace, then check whether a faulted code/rodata page is ever mapped with
   wrong/stale content (compare mapped PPN bytes vs the file), and audit the
   async user-page-fault path (`hand_off_user_pf` → reschedule → resolve →
   `enter_userspace_with_context`) for a race where re-entry happens before the
   page content is materialised, or a pmap-root reclaim during the fault. The
   `fptest01`-style **deterministic** failure suggests a deterministic mapping
   bug, not a true race.
2. **Latent FP-save asymmetry (separate, found en route, NOT this bug).** The
   trap asm SAVES FP when `FS>=2` but RESTORES when `FS!=Off` — so a thread
   trapping at FS=Initial skips the save yet restores the stale per-hart frame
   FP. No current test hits it (no FS=Initial traps observed for the sender),
   but it is a real bug. Fix = make ENTRY save match RESTORE: in
   `tx_rv64_qemu_minimal_trap_vector`, change `li t1,2; bltu t0,t1,1f` to
   `beqz t0,1f`. (Tested: no FP regression in basic tests; did NOT fix
   broken_ip, confirming the asymmetry is orthogonal.)

## Bottom line for scoring

broken_ip ×8 (~47 pts/lane) stays blocked behind a VM-under-load correctness
bug, not a net/clock/argv bug. It is a deeper subsystem fix than the prior
handoff anticipated. The rv tier-1 bankings (~95 pts/lane) are unaffected.
