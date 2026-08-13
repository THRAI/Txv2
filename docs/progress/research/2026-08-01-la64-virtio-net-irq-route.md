# LA64 virtio-net IRQ route and GitHub clone recovery

Date: 2026-08-01

Branch: `feature-network-refactor-recovery`

Baseline commit: `067381d145c8`

## Finding

The LA64 GitHub hang was caused by an uninitialised PCH-PIC HTMSI vector table,
not Git, TLS, TCP, ext4, the allocator, or a general timer failure.

QEMU's LoongArch `virt` GPEX host swizzles PCI INTx into PCH-PIC inputs 16–19.
With the board profile pinned to block slot 1 and virtio-net slot 2, the net
device's INTA maps to PCH-PIC input 18 and therefore txKernel public GSI 82.
QEMU resets every PCH-PIC HTMSI vector entry to zero, so merely unmasking
PCH input 18 and ExtIOI 18 delivered the asserted source to ExtIOI 0 instead.

Primary implementation references:

- [QEMU 9.2.1 LoongArch virt PCI routing](https://gitlab.com/qemu-project/qemu/-/blob/v9.2.1/hw/loongarch/virt.c)
- [QEMU 9.2.1 LoongArch PCH-PIC](https://gitlab.com/qemu-project/qemu/-/blob/v9.2.1/hw/intc/loongarch_pch_pic.c)
- [QEMU 9.2.1 LoongArch ExtIOI](https://gitlab.com/qemu-project/qemu/-/blob/v9.2.1/hw/intc/loongarch_extioi.c)

## Witness

Before the fix, `bash tools/diagnose-la64-time-dns.sh github-ls-remote`
completed DNS, proxy CONNECT, TLS 1.3, certificate validation, and the HTTP
request send, then received no response header. The guest timeout could not
resume because the CPU was in WFI with `TCFG=0`.

The decisive QEMU trace was `/tmp/la64-focus-xPRsmK/qemu-trace.log`:

- virtio-net asserted PCH-PIC input 18;
- PCH input 18 was unmasked and pending;
- PCH-PIC raised ExtIOI 0 because HTMSI vector entry 18 was still zero;
- txKernel had enabled ExtIOI 18, so the CPU received no usable network IRQ.

The identical external request passed on RV64, and forcing HTTP/1.1 on LA64
did not change the failure. Those A/B runs rejected an upstream proxy outage
and an HTTP/2-only bug.

## Resolution

- Publish LA64 `NET_IRQ = 82` for the proven slot-2 INTA route.
- When unmasking a PCH-backed source, enable the same-numbered ExtIOI line,
  program `PCH_PIC_HTMSI_VECTOR[pin] = ext_irq`, order the MMIO writes, and only
  then expose the PCH input.
- Enable virtio-pci network notifications only after `eth0` publication, using
  the existing deferred top-half/bottom-half completion protocol.
- Emulate destructive claim semantics at the ExtIOI boundary. The first
  presentation records software ownership without changing the proven route.
  If the non-destructive pending bitmap presents that source again before
  completion, acknowledge the duplicate and suppress its ExtIOI delivery line;
  the original owner's `complete` restores delivery. This prevents a deferred
  level-triggered IRQ from being published twice before its bottom half runs.
- Pin LA64 block/net PCI slots to 1/2 in maintained launchers so the static
  board fact cannot drift with QEMU device ordering.
- Extend the LA64 Git gate with the RV64-style opt-in IRQ counter assertion and
  remove each temporary guest disk on harness exit.

The 10 ms network poll watchdog remains enabled as a backstop; this change does
not move network semantics into HAL or IRQ context.

## Verification

- `cargo test -p tx-hal-loongarch64-qemu-virt`: 55/55.
- `cargo xtask build --target la64-qemu`: passed.
- Post-fix QEMU trace `/tmp/la64-focus-lGX1vq/qemu-trace.log` shows PCH input
  18 delivered as ExtIOI 18 and completed as bit `0x40000`.
- Default-protocol GitHub `ls-remote`: rc 0, serial
  `/tmp/la64-focus-7i7hL3/serial.log`.
- Full `git clone https://github.com/oscomp/xv6-riscv.git`: 7780 objects,
  17.46 MiB, 4127 deltas, rc 0, HEAD
  `f5dea58cc1057f2b076cdb90b446c2c21d91171e`; serial
  `/tmp/la64-focus-rQcE8O/serial.log`.
- `TX_REQUIRE_NET_IRQ=1 TX_GIT_NET_TIMEOUT=360 bash
  tools/verify-git-net-la64.sh`: 9/9, `claims=79`, `completions=79`,
  `wrong-hart=0`, `missing-device=0`; serial
  `/tmp/verifygit-hIv6bt/serial.log`.
- `cargo test -p xtask qemu`: 33/33; `cargo test -p xtask oscomp`: 2/2.

Post-main-merge verification exposed one controller-semantics gap: ExtIOI's
pending bitmap can present the same level source again before a deferred bottom
half completes, unlike a destructive PLIC claim. The retained software logical
claim gateway acknowledges and suppresses only that duplicate presentation.
After the fix, the LA64 Git/IRQ gate passed 9/9 with 104 claims, 104
completions, and zero wrong-hart or missing-device drains; both LA64 netperf
lanes and both iperf lanes passed 22/22 in total. See
`2026-08-01-main-merge-network-validation.md`.

## Next and blockers

The reported clone hang has no remaining blocker. LA64 netperf/TCP_CRR and the
previously recorded publication/EBR issue remain separate work and are not
made complete by this Git/IRQ repair.
