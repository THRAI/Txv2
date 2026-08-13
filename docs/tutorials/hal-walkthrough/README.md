# Building a Portable Kernel HAL — the txKernel axHal Model

A tutorial walkthrough of txKernel's Hardware Abstraction Layer, told through the
RISC-V 64 QEMU `virt` board.

## Who this is for

You know the classic operating-systems vocabulary — page tables, trap vectors,
IRQ controllers, the kernel/user privilege split — but you want to see how a
*type-safe, statically dispatched, portable* HAL is actually built in Rust, with
the design decisions made explicit. Each chapter takes a traditional OS topic,
shows the txKernel design decision for it (with the `txdoc:` anchor from
[`HAL_v1.md`](../../design/01_substrate/HAL_v1.md)), then walks the real code in
`crates/tx-hal`, the board crate `boards/tx-hal-riscv64-qemu-virt`, and the
generic kernel in `crates/tx-kernel`.

## A note on truth

This tutorial is faithful to the **code as it ships today**, not to the design
document where the two have drifted. `HAL_v1.md` is the architectural intent;
several load-bearing details have moved on since it was written. Wherever the
code outgrew the doc, you will find a **Divergence** callout that names both the
doc's claim and the shipped reality, and explains why. The full list is collected
in [Appendix A](appendix-a-design-vs-code-ledger.md) so the tutorial doubles as a
doc-drift audit.

## Reading order

### Part I — Foundations
- [Chapter 1 — What a HAL is, and what txKernel refuses to be](ch01-what-a-hal-is.md)
- [Chapter 2 — The `TxPlatform` supertrait and zero-sized dispatch](ch02-txplatform-supertrait.md)
- [Chapter 3 — The address dialect: values vs dereference authority](ch03-address-dialect.md)

### Part II — Boot
- [Chapter 4 — From firmware reset to Rust (H0–H1)](ch04-firmware-to-rust.md)
- [Chapter 5 — Typestate as a boot-safety guard (`BootStaticBag`)](ch05-bootstaticbag-typestate.md)
- [Chapter 6 — The portable handoff shell and the real mainline (H2–H3)](ch06-handoff-shell-mainline.md)

### Part III — Memory (pmap)
- [Chapter 7 — Page tables behind a proof-object API](ch07-pmap-proof-objects.md)
- [Chapter 8 — Kernel half vs process roots](ch08-kernel-half-vs-process-roots.md)

### Part IV — Traps, syscalls, and the return to userspace
- [Chapter 9 — Trap entry and the shell-to-sink contract](ch09-trap-shell-to-sink.md)
- [Chapter 10 — Why divergent-into-userspace returns: the stackless-coroutine trick](ch10-stackless-coroutine-return.md)
- [Chapter 11 — User-memory access: eager walk and the surviving fixup exception](ch11-user-access-fixup.md)
- [Chapter 12 — Signals across the ABI boundary](ch12-signals-abi.md)

### Part V — Devices, time, SMP
- [Chapter 13 — Interrupts: explicit registration, not link-time magic](ch13-irq-explicit-registration.md)
- [Chapter 14 — Time, per-CPU state, and SMP mechanics](ch14-time-percpu-smp.md)

### Part VI — Synthesis
- [Chapter 15 — Porting to a second board](ch15-porting-second-board.md)
- [Appendix A — Design-vs-code ledger](appendix-a-design-vs-code-ledger.md)
- [Appendix B — Debugging traps with `cargo xtask fault-decode`](appendix-b-fault-decode.md)

## Conventions

- File and line references are written as `path:line` and were accurate at the
  time of writing; treat them as starting points, not eternal truth.
- Code blocks are quoted from the tree. Long listings are trimmed with `// …`
  and the trim is always called out.
- "The doc" means [`HAL_v1.md`](../../design/01_substrate/HAL_v1.md); section
  references use its `txdoc:` anchors.
</content>
</invoke>
