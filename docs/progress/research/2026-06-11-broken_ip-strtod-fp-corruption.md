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

## VM dive (2026-06-11, continued) — narrowing, what's ruled out

The QEMU runs **`-smp 1` (single hart)**, so this is NOT a multi-hart race
(cross-hart pmap-root reclaim / concurrent frame allocation are impossible).
It is a **single-hart, deterministic, cooperative-async** fault-path bug.

Ruled out by experiment:
- **Not memory pressure / page-cache eviction:** `-m 4G` (vs `-m 1G`) still
  hangs (0 TPASS). More RAM doesn't help.
- **Not simple concurrent file-backed faulting:** a binary with 6 children
  looping `mmap(/lib/ld-musl…, MAP_PRIVATE)` + touch-256-pages + `munmap`
  while the parent runs `strtod("2")` 200 000× → **0 corruptions**.
- **Not plain exec-churn:** a harness with 4 children looping
  `fork+exec(self,"x")+wait` while the parent repeatedly `fork+exec`'d a fresh
  `strtod("2")` child → reached 512+ child submits with **no child hang/wrong
  result** (didn't reproduce; 4000-run harness too slow under TCG to finish).

Key constraint: **my recompiled `ns-icmpv4_sender` PASSES in the exact same
broken_ip netns context** (see above). So the trigger is the **image binary's
specific fault pattern × the specific pre-sender system state** built by the
broken_ip setup (netns + veth + many `ip`/netlink ops + the larger 58 KB
binary's cold-page faults), not concurrency or exec-churn alone. **It does not
reproduce in isolation** — the corruption needs that exact in-situ state.

Demand-paging architecture map (for the in-situ instrumentation step):
1. `crates/tx-kernel/src/trap.rs:13` `on_page_fault` → `trap_handoff.rs:279`
   `hand_off_user_pf` (capture ctx, resolve wait — synchronous, ctx preserved).
2. `crates/tx-kernel/src/thread_future.rs:909` `aspace.fault_script(fault).await`
   (resolves fully before re-entering userspace).
3. `crates/tx-subsystems/src/vm/execution.rs:255` `fault_script_with_ufd_dispatch`
   → `try_fault_script_resolve` (Materializer range-lock + `require_fault_recipe`)
   → `try_fault_script_materialize_and_publish` (loop, await_range_lock on Wait).
4. File-backed page: `crates/tx-subsystems/src/page_backed/mod.rs:806`
   `materialize_file_page` (offset = `page*4096`, `fetch_page`, Owner/Joined
   handshake) → `:971` `install_fetched_file_page_from_owner` (page cache).
5. PTE publish: `crates/tx-subsystems/src/vm/pmap.rs:237`
   `publish_page_with_replacement` → `(ops.commit_mapping)(root, reservation,
   perms)`.

## Concrete next step (in-situ instrumentation — the bug won't repro otherwise)

Re-add a `tx_hal` flag set when an `icmpv4_sender` execs (as the diagnosis
probes did), and in the BOARD pmap commit (which has console access) log
`(fault_va, ppn)` **and the first 8–16 bytes of the just-mapped frame** for the
sender's faults. Run the real broken_ip-version.sh and look for: a ppn aliased
to two distinct VAs in one address space; a code page whose mapped bytes are
not valid RISC-V / differ from the libc file; or a frame whose content changes
after mapping (reuse-while-mapped). Cross-reference with `emit_vm_trace`
(`debug.vm.fault.*`) which is already wired through the observe system. The
deterministic failure ⇒ a deterministic mapping/content bug, not a pure race.

## In-situ instrumentation results (2026-06-11, round 2) — corruption is in WRITABLE data

Built the probe above (flag in `tx_hal` set on `icmpv4_sender` exec; logger in
`boards/.../pmap/address_space.rs::commit_mapping_from_root`, which has VA, PA,
perms via `reservation`, and frame content via `topology::direct_map_virt`).
Ran the real broken_ip-version.sh (image sender, hangs). Findings:

- **Sender read-only (code/rodata) page commits look VALID at commit time.**
  76 read-only USER commits; the strtod code page that *was* re-committed
  (`va=0x3e007ca000`) holds valid RISC-V (`b0=4086843b0885cc63 …`). Only two
  pa's were aliased to 2 VAs each, both legitimate: an ELF segment-boundary
  page (adjacent VAs, same file page) and a shared zero page.
- **A content-change detector (record each sender read-only frame's first 16
  bytes at commit; re-verify all recorded frames on every later commit) fired
  `VMCHANGED = 0`.** So no sender code/rodata frame is reused-while-mapped or
  otherwise corrupted after mapping.
- Combined with the earlier register-preservation proof: **strtod executes
  correct code with correct rodata constants and preserved registers, yet
  returns 0.0.** Therefore the corruption is in **WRITABLE data** — the input
  `"2"` string (in argv on the initial stack) that strtod parses, or the
  long-double soft-float's **stack spills** — most likely an **anon writable
  page mapped to a reused frame** (or the argv page corrupted). NOTE: most of
  the strtod *execution* pages from the FP trace were NOT re-committed after the
  sender exec (they were shared-cached from earlier in boot), so a from-boot
  log would be needed to cover those too.

Next instrumentation (round 3): widen the probe to WRITABLE USER commits and
detect anon-page frame reuse (a pa committed for two distinct anon-writable VAs
without an intervening unmap — illegitimate for private anon), and/or dump the
sender's argv `"2"` string region at the first AF_PACKET sendto (sender already
spinning, strtod done) to confirm whether the *input* was corrupted vs a spill.
Focus the VM fix on the anon/private-page materialization + frame-allocation
path rather than the file-backed/page-cache path (the latter is verified clean).

## Round 3 + TLB test (2026-06-11) — mappings are fully correct; NOT a stale TLB

- **No frame reuse / aliasing.** Logged EVERY user-page commit (va, pa, perms)
  after the sender execs (86 commits — the test is hung waiting, so little
  concurrent activity during strtod). Only 3 physical frames were mapped to >1
  va, all **legitimate**: a shared zero page, an ELF segment-boundary page
  (adjacent VAs, same file page), and one RO→RW **CoW** permission change on a
  single va. The X∩W "intersection" is just the 3 RWX stack pages (the stack is
  mapped executable for the signal trampoline). So **the page tables and frame
  contents are entirely correct.**
- Also confirmed **argv is intact**: the `fake_p` dump shows saddr/daddr/MAC all
  correct (parsed from -S/-D/-M by getopt+inet_pton), so the strtod **input
  `"2"` is not corrupted** either.
- **Stale-TLB hypothesis REFUTED.** `activate_user_pmap` writes `satp` without an
  `sfence.vma` on address-space switch (an optimization that removed the old
  unconditional flush; comment at `boards/.../lib.rs` ~476-488). Re-added a
  per-switch `sfence.vma` → broken_ip **still hangs** (0 TPASS). So it is not a
  stale TLB on switch (and under QEMU TCG a `satp` write flushes anyway).

So: correct code, correct rodata constants, correct input, preserved registers,
correct page tables, no TLB staleness — **yet `strtod("2")` returns 0.0.** The
remaining candidates are a **stack-spill of the long-double soft-float** that is
re-faulted/lost across a cold-page fault at a precise moment, or a libc-internal
data path, both extremely timing-specific. After ~6 instrumentation rounds the
exact faulting step has not been isolated by static logging.

## Recommended definitive next tool: QEMU gdbstub single-step

Static commit/content logging has exhausted its usefulness. The decisive next
step is **QEMU `-s -S` gdbstub** (as used for the route4 livelock): break at the
sender's `strtod` return / the `fsd fa0,1568(s0)` store in `parse_options`
(`0x…15d8` + PIE base), single-step the soft-float, and watch the exact
instruction where the running value diverges from 2.0 — then correlate that VA
with `info mem` / the page tables. That pinpoints whether it's a spill reload, a
specific FP op, or a memory read returning the wrong byte, which static logging
cannot see.

## Bottom line for scoring

broken_ip ×8 (~47 pts/lane) stays blocked behind a VM-under-load correctness
bug, not a net/clock/argv bug. It does NOT reproduce outside the exact
broken_ip context, so the next step is in-situ instrumentation of the real run
(above), not a standalone repro. It is a deep, possibly multi-session VM fix.
The rv tier-1 bankings (~95 pts/lane) are unaffected.
