# broken_ip sender hang — REAL root cause: image musl float-ABI mismatch (NOT a kernel bug)

Date: 2026-06-11 (corrected — supersedes the earlier "VM demand-paging
corruption" conclusion in this same file, which was **wrong**)
Lanes: fails on **rv.musl / la.musl**; **PASSES on rv.glibc / la.glibc**.
Status: **SOLVED. Not a kernel bug.** broken_ip is scorable on the glibc lanes.

## TL;DR

`broken_ip-*` hangs **only on the musl lanes** because the OSComp image ships an
**inconsistent musl toolchain**: the image's `/musl/lib/libc.so` is built
**soft-float** (lp64, `e_flags=0x0`, doubles returned in `a0`), but the LTP test
binary `ns-icmpv4_sender` is built **hard-float** (lp64d, `e_flags=0x5`, doubles
read from `fa0`). So when the sender calls `strtod("2")` for its `-t 2` timeout:

- musl's soft-float `strtod` → `__trunctfdf2` **correctly computes 2.0 and
  returns it in `a0`** (the soft-float ABI return register).
- the hard-float sender reads its `double` result from **`fa0`**, which is `0`.

Result: `fake_p->timeout = 0.0`, the sender's send loop guard
`if (fake_p->timeout) if (fake_p->timeout < difftime(...)) break;` is never
true, and the loop spins on `sendto` forever → the script never returns → 0 TPASS.

```c
for (;;) {
    sendto(...);                 // AF_PACKET, returns immediately
    if (fake_p->timeout)         // <-- 0.0 on musl (result stranded in a0), guard false
        if (fake_p->timeout < difftime(time(NULL), start_time)) break;
    if (catch_sighup) break;
}
```

This is an **image build inconsistency** (soft-float libc vs hard-float test
binaries). It fails on **any** kernel for the musl lane and is **not fixable in
the kernel**. The glibc lane ships a consistent hard-float toolchain
(`libc.so.6` and the glibc-lane `ns-icmpv4_sender` are both `e_flags=0x5`), so
broken_ip works there.

## Decisive evidence

1. **Float-ABI ELF flags (the root):**
   - image `/musl/lib/libc.so` → `file`: *"soft-float ABI"*, `readelf -h` flags
     `0x0`. Its `__trunctfdf2` (quad→double) ends `… ; ret` returning **`a0`**,
     with **no `fmv.d.x fa0,a0`** — it never places the result in an FP register.
   - image `/glibc/lib/libc.so.6` → *"double-float ABI"*, flags `0x5`.
   - image glibc-lane `ns-icmpv4_sender` → *"double-float ABI"*, flags `0x5`
     (consistent with its libc → works).
2. **gdbstub smoking gun (musl lane).** Breaking at musl `strtod`'s return and
   reading both registers:
   `a0 = 0x4000000000000000` (**exactly the IEEE-754 bit pattern of 2.0**) while
   `fa0 = 0x0`. The value was computed correctly — it is simply in `a0`, not
   `fa0`. The sender reads `fa0` → sees 0.0. (This is the same observation the
   earlier draft of this note misread as "the soft-float compute is corrupt.")
3. **End-to-end (this session).** `tx.oscomp.groups=ltp-bin:glibc:broken_ip-version.sh`
   → `Summary: passed 6` (5 sender sizes + final ping), `TPASS` on every line.
   The identical run on the **musl** lane hangs at the first sender with 0 TPASS.

## What it is NOT (earlier hypotheses, all refuted)

- **NOT busybox `-t` arithmetic / wrong argv.** Kernel execve-argv dump shows the
  sender invoked with literal `-t 2`; `tst_net.sh`'s `$((10/5))` = 2.
- **NOT a frozen CLOCK_REALTIME / vDSO.** The live exec path advertises no vDSO
  (`at_sysinfo_ehdr: None`), so libc uses the `clock_gettime` *syscall*, which
  returns advancing realtime (verified by a kernel heartbeat).
- **NOT a kernel VM / demand-paging corruption.** Six rounds of in-situ
  instrumentation (commit-time VA/PA/perms/content logging, frame-reuse and
  content-change detectors, a per-switch `sfence.vma` test, and a Python-gdb
  single-stepper) found **correct code, correct rodata constants, correct input,
  preserved registers, correct page tables, no TLB staleness** — because there
  was never a kernel bug. The soft-float computed 2.0 correctly; the result was
  just in the wrong register for the hard-float caller. The whole "VM-under-load"
  theory was chasing the wrong register.
- **NOT reproduced by my recompiled sender.** That comparison was invalid: my
  host `riscv64-linux-musl-gcc` is effectively a hard-float toolchain, so the
  binary I built matched whatever libc I linked and didn't exhibit the image's
  cross-ABI split.

## Scoring impact

- **broken_ip ×8 is scorable on the glibc lanes** (rv.glibc, la.glibc) — a
  consistent hard-float toolchain. No kernel change required. **rv.glibc witness
  = `48/48` (every file 6/6)** — `target/oscomp/ltp-bin/bipglibc-rv.glibc.judge`.
- **broken_ip ×8 is unscorable on the musl lanes** — image soft-float-libc vs
  hard-float-test-binary mismatch; fails on any kernel; out of our control.
- Per-file judge weights: all eight at 6/6 → **48 pts per glibc lane** (the net
  ledger's old "47" undercounted nexthdr, which the judge scores 6, not 5).

## How to verify / reproduce

Run the official-form witness (boots our submit kernel with the no-args
`ltp-bin` group and scores with the real `tools/oscomp-judge.py`):

```sh
BIP="broken_ip-version.sh+broken_ip-ihl.sh+broken_ip-checksum.sh+broken_ip-plen.sh+broken_ip-protcol.sh+broken_ip-dstaddr.sh+broken_ip-fragment.sh+broken_ip-nexthdr.sh"
tools/ltp-bin-witness.sh 1300 rv.glibc "$BIP" bipglibc   # → PASS
tools/ltp-bin-witness.sh 1300 rv.musl  "$BIP" bipmusl    # → hang/0 (image ABI bug)
```

Confirm the ABI split directly (extract the libc + a sender from the image via
`tune2fs -O ^metadata_csum` + `debugfs -R "dump <path> <out>"`, then):

```sh
file musl_libc.so       # → "... soft-float ABI"
readelf -h musl_libc.so | grep Flags        # → 0x0
file glibc_ns-icmpv4_sender  # → "... double-float ABI"  (flags 0x5)
```

## Latent FP-save asymmetry (found en route, orthogonal — NOT this bug)

The rv64 trap asm SAVES the FP frame when `FS>=2` but RESTORES when `FS!=Off`,
so a thread trapping at `FS=Initial` would skip the save yet restore a stale
per-hart FP frame. No current test hits it, but it is a real latent bug. Fix =
make ENTRY save match RESTORE: in `tx_rv64_qemu_minimal_trap_vector`
(`boards/tx-hal-riscv64-qemu-virt/src/trap.rs`) change `li t1,2; bltu t0,t1,1f`
to `beqz t0,1f`. (Tested: no FP regression; did not change broken_ip, confirming
it is unrelated.)

## Probe / repro notes (reusable)

- The image uses ext4 `metadata_csum`; strip it before `debugfs`/`e2fsck`:
  `cp …/sdcard-rv.img /tmp/sd.img; tune2fs -O ^metadata_csum /tmp/sd.img;
  e2fsck -fy /tmp/sd.img`. LTP bins live at `/{musl,glibc}/ltp/testcases/bin`.
- gdbstub harness: `qemu-system-riscv64 … -S -s` (halted, gdbstub on :1234) +
  `gdb-multiarch --batch -x script` with `set architecture riscv:rv64;
  target remote :1234`. Write gdb scripts into the **repo** dir (a /tmp-cleaner
  deletes them mid-session); start QEMU with run_in_background then drive gdb in a
  separate foreground call (avoid `( qemu ) &` + port-:1234 reuse races); only
  **2 hardware breakpoint triggers** exist (`sdtrig`) and software breakpoints
  cannot be set on unmapped/cold pages, so step cold pages by
  `hbreak *($pc+2); hbreak *($pc+4); continue`.
