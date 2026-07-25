# Appendix B — Debugging traps with `cargo xtask fault-decode`

When a kernel goes wrong on the RV64 QEMU board, the symptom is usually a trap: a
page fault at an address you didn't expect, an illegal instruction, or — the
classic bringup failure from Chapter 4 — a fault the instant `satp` turns on that
loops in the trap vector. The raw evidence is three CSRs: `scause`, `sepc`, and
`stval`. `cargo xtask fault-decode` turns those numbers into a symbolized,
human-readable diagnosis. This appendix is a short field guide.

## What it does

`fault-decode` reads the kernel ELF's symbols and DWARF line info (via
`addr2line`) and the board's address-space layout, then for a given fault it tells
you:

- the **trap class** decoded from `scause` (the same classification as Chapter 9's
  `classify_rv64_trap`);
- the **`sepc`** symbolized to `function (file:line)` — *where* the fault happened;
- the **`stval`** interpreted against the board's VA layout (Chapter 3) — is the
  faulting address in the user half, the direct map, the kernel image alias, or
  nowhere valid?

It knows the RV64 register names and the Sv39 windows (`RV64_USER_TOP`,
`RV64_SV39_BITS`, the 512 MiB kernel window) from `xtask/src/fault_decode.rs`.

## Three ways to invoke it

The usage string (`xtask/src/lib.rs:105`):

```
cargo xtask fault-decode --target rv64-qemu [--elf PATH]
    [--serial PATH [--all] | --scause HEX --sepc HEX --stval HEX | --addr HEX]
```

**1. From a captured serial log.** The most common path — point it at a saved
serial capture and it scans for fault dumps and annotates each:

```sh
cargo xtask fault-decode --target rv64-qemu --serial target/serial.log --all
```

(`--all` decodes every fault in the log; without it, the first.)

**2. From raw CSR values.** When you have `scause`/`sepc`/`stval` from a panic line
or a debugger:

```sh
cargo xtask fault-decode --target rv64-qemu \
    --scause 0xd --sepc 0xffffffff80203abc --stval 0x40
```

(`scause = 0xd = 13` is a load page fault; see the Chapter 9 cause table.)

**3. From a single address.** To symbolize one `sepc` or pointer:

```sh
cargo xtask fault-decode --target rv64-qemu --addr 0xffffffff80203abc
```

By default it locates the kernel ELF for the target; `--elf PATH` overrides (handy
for decoding against an older build).

## It's automatic in `cargo xtask qemu`

You rarely have to run it by hand. The QEMU runner annotates faults inline: when a
run traps, `fault_decode_annotation` (`xtask/src/qemu.rs:630`) feeds the serial
log through the same decoder and prints the symbolized diagnosis right after the
fault, so a failing `cargo xtask qemu` run shows you `function (file:line)` for the
fault without a second step.

## Reading the output: the Chapter 4 bug

The canonical bringup failure (Chapter 4): you grow the kernel past the 32 MiB
`KERNEL_BOOTSTRAP_ALIAS_SIZE` window, and the instant `satp` turns on, the PC is in
an unmapped part of the kernel image — instant instruction page fault, looping in
the vector. `fault-decode` makes this obvious:

- **trap class:** `InstructionPageFault` (`scause = 12`);
- **`sepc`:** a kernel-alias address *above* `KERNEL_VIRT_BASE + 32 MiB`, which the
  layout interpreter flags as "in the kernel window but beyond the bootstrap
  alias";
- **`stval`:** equals `sepc` (the fetch address), confirming it's the fetch itself
  faulting, not a data access.

The fix — bump `KERNEL_BOOTSTRAP_ALIAS_SIZE` and the matching
`TX_RV64_KERNEL_ALIAS_L0_TABLES` (`topology.rs:54`, `boot_trampoline.rs`) — falls
right out of that reading.

## How it connects to the tutorial

`fault-decode` is the operational mirror of the design this tutorial walked:

- It decodes `scause` with the same table as Chapter 9's `classify_rv64_trap`.
- It interprets `stval` against the same Sv39 windows as Chapter 3's address
  dialect (`DIRECT_MAP_BASE`, `KERNEL_VIRT_BASE`, `USER_TOP`).
- It knows the boot-time kernel window from Chapter 4, which is why it can name
  the "beyond the bootstrap alias" failure mode.

Once you understand the boot mappings and the trap classification, `fault-decode`
output reads like prose. That's the point of the tool — and, in a sense, the point
of the whole tutorial: the HAL's structure is regular enough that a fault can be
explained in terms of a small, knowable set of windows and causes.

---

*End of the HAL walkthrough. Return to the [index](README.md).*
</content>
