# netperf UDP_RR / TCP_CRR flaky user-segv (pc=0x2000) — characterization

Date: 2026-07-10
Branch: feature-network-refactor
Status: **characterized, NOT fixed** (deliberately parked — low priority)

## Symptom

Under the `tx.runsh` ext4 flow, running netperf `UDP_RR` / `TCP_CRR` in a loop,
~10% of individual tests crash with a userspace segfault:

```
user-segv:pf:access=exec:err=no-recipe:pc=0x2000:addr=0x2000:ra=0x2000:sp=0x40xxxxxx:a0=0xe
```

- `scause=0xc` (instruction page fault), `sepc=stval=0x2000`.
- Flaky (~10% per RR/CRR test), **TCG-only, retry passes** → low severity, does
  not materially affect OSComp scoring (official form is a single `-l 1` run).
- STREAM tests never hit it; only the request/response (blocking-recv) tests.

## What is SOLID (each experimentally proven)

1. **Crash = netperf executes `ret` through a corrupted stack return-address.**
   gdb-decoded `saved_user_context`: `pc == ra == 0x2000` (the `ret`/`jalr x0,ra`
   pattern), `a7 == 198` (`__NR_socket`), `a0 == 14` (the fd `socket()` returned).
   i.e. netperf crashes right after a `socket()` in the CRR loop, `ret`-ing to a
   clobbered slot = `0x2000`.
2. **Stack corruption, NOT register/context corruption.** Site-tagged probes on
   the two context-write paths (`restore_sigreturn_frame`, signal handler entry
   `store_saved_user_context`) NEVER stored a `0x2000` context. So the bad value
   lives on the stack, loaded into `ra` at the `ret`, not injected into a saved
   register context.
3. **SIGALRM is the necessary trigger.** netperf `-l N` (N>0) uses an itimer
   (SIGALRM) to end the timed test. Switching to `-l -N` (transaction count, NO
   SIGALRM) → **0 crashes across 60 iterations**; `-l 1` → crashes. Removing the
   signal removes the crash.

## Falsified hypotheses (7)

| # | Hypothesis | How falsified |
|---|---|---|
| 1 | Register/context corruption in signal delivery | site probes: 0 stores of a 0x2000 pc/ra |
| 2 | Signal frame written at a stale sp (overlap) | placement uses fresh `saved_user_context().sp`; recurring frames are different tests, not live-simultaneous |
| 3 | Alt-stack (sigaltstack) overflow | netperf disassembly: **no `sigaltstack`**, all handlers on the main stack |
| 4 | Frame content carries 0x2000 (a saved reg) | reg-scan of every SIGALRM `orig_ctx`: **REG2000 = 0** |
| 5 | setjmp/longjmp signal dance | netperf disassembly: **no setjmp/longjmp** |
| 6 | The SIGSEGV storm (0x11ced8) is causal | it's netperf-normal memory probing — **282 SIGSEGVs with OR without SIGALRM**, `0x11ced8` isn't even in netperf's `.text` (different process); red herring |
| 7 | "last SIGALRM before crash" is the corruptor | crash is **delayed** from the corrupting frame; per-iteration new process + drifting libc base make opc↔crash correlation unreliable |

## Extra evidence gathered

- Stack VMA is `rwx` (executable — the on-stack sigreturn trampoline needs it).
- Crash `sp` is **near the stack top** (shallow, ~2 KiB in) while signal frames
  land deep → a deep frame does not directly overlap the shallow crash slot.
- Crash-process libc code VMA identified (e.g. `0x3e00a94000-0x3e00b37000`); opc
  absolute addresses drift per-iteration (sequential mmap, no ASLR reuse).

## Verdict + only remaining path

This is a deep, **multi-process, delayed-manifestation** signal-timing race:
SIGALRM frame delivery corrupts netperf's main stack, but the exact byte-write
of `0x2000` was not caught by instrumentation+correlation (7 hypotheses dead).

The one remaining rigorous path: a **hardware watchpoint on the corrupted stack
slot** to catch the write — requires per-process user-VA → phys translation
(gdb reads the kernel satp at the fault, so user VAs are not directly readable),
i.e. a dedicated deep effort. Parked as poor ROI for a flaky/TCG-only bug.

## How to reproduce / verify

- Disk: busybox ext4 (`TX_OSCOMP_RISCV_MUSL_DIR=.../testcase/riscv/musl` build)
  + a `tx-run.sh` looping `netperf ... -t UDP_RR -l 1` and `-t TCP_CRR -l 1`.
- Boot `tx.runsh=/musl/tx-run.sh` (no `TX_BUSYBOX` on the kernel — that bakes a
  `busybox_baked` panic in `register_busybox_into_tmpfs`; the ext4/`tx.runsh`
  flow does not need it).
- ~60 iterations yields a handful of `user-segv:pf ... pc=0x2000` lines.
- Control: same loop with `-l -300` (no SIGALRM) → 0 crashes.

## Related

- The `:ra=0x…:sp=0x…:a0=0x…` fields on the `user-segv` line were added this
  session (`log_user_segv`, thread_future.rs) and kept — useful for any future
  user-crash triage.
- REVERSE_UDP large-datagram corruption (separate, FIXED): root cause was the
  4 KiB socket read/write inline cap in `io.rs` (`SOCKET_IO_MAX_INLINE`), with a
  `udp` ring floor of 65536; see `[[netperf-iperf-refactor-branch]]`.
