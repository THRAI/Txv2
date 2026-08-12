# Chapter 4 — From firmware reset to Rust (H0–H1)

This is the chapter most kernel tutorials skip: the assembly that runs between
"the firmware released the hart" and "Rust code is running at its linked virtual
address." txKernel names the boot stages H0 through H4 (`txdoc:HAL-THE-BOOT-SEQUENCE-1`).
H0 is the firmware handoff; H1 is the pre-Rust bootstrap. Both live entirely in
the board crate.

## H0 — the firmware contract

On RV64 QEMU `virt`, OpenSBI runs in M-mode and jumps to our kernel in S-mode at
physical `0x8020_0000`, with:

- `a0` = hart id
- `a1` = pointer to the device tree blob (DTB)

That is the whole contract. The doc tabulates it at
`txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H0-…-1`, and the crucial rule is that the
*meaning* of `a0`/`a1` is platform-owned. The generic kernel never learns that
"`a1` is a DTB"; the board translates it into a typed `BootHandoff` (Chapter 6).
H0 ends when control reaches the platform's `_start` symbol.

## The linker script: two addresses for every byte

The defining fact of this board's boot is that the kernel is **linked high but
loaded low**. The linker script `linker-rv64-qemu-virt.ld` sets:

```
KERNEL_LOAD_BASE = 0x80200000;                  /* where OpenSBI puts us */
KERNEL_VIRT_BASE = 0xffffffff80200000;          /* where the kernel is linked */
KERNEL_VIRT_OFFSET = KERNEL_VIRT_BASE - KERNEL_LOAD_BASE;   /* 0xffffffff00000000 */
```

Every section is *linked* at its high VMA but *loaded* (`AT(…)`) at the
corresponding low physical address. So `__text_start` resolves to a high
`0xffff_ffff_80…` address, while `__text_start_load` resolves to the low
`0x8020_0…` address. The script exposes a `_load`-suffixed symbol for every
boot-relevant linker symbol precisely because the trampoline runs *before* the
MMU is on, when only the low addresses are valid.

The sections that matter for boot:

- `.text.trampoline` — the boot assembly, kept first and at the load base. It
  runs with the MMU off, so it must be position-correct at the low address and
  may only touch `_load` symbols.
- `.bss` — includes the **statically reserved boot page tables**:
  `__bootstrap_root`, `__kernel_alias_l1`, `__kernel_alias_l0_tables`, and the
  `__pt_node_pool`. These are carved out of BSS so the trampoline can fill them
  with no allocator.
- `.bss.stack` — the boot stack (`__tx_boot_stack_bottom` / `__tx_boot_stack_top`).

Reserving the page tables as linker symbols in BSS is the trick that lets H1 build
a page table with nothing but stores to known addresses — no heap, no allocator,
not even a frame.

## H1 — `_start`, step by step

The whole trampoline is one `global_asm!` block in `boot_trampoline.rs`. It opens
with a wall of `.equ` constants that mirror the Rust-side topology (the comments
warn you to keep them in sync — e.g. `TX_RV64_KERNEL_ALIAS_L0_TABLES = 16` must
equal `topology.rs::KERNEL_ALIAS_L0_TABLES`). The PTE flag equates are the Sv39
bits we'll meet again in Chapter 7:

```
.equ TX_RV64_PTE_IDENTITY,    V | R | W | X | A | D     /* RWX leaf, for the low bridge */
.equ TX_RV64_PTE_DIRECT,      V | R | W | G | A | D     /* RW global leaf, no execute */
.equ TX_RV64_PTE_KERNEL_BOOT, V | R | W | X | G | A | D /* kernel image leaf */
```

Now `_start` (`boot_trampoline.rs:43`). Read it as five movements.

### 1. Per-hart stack

```asm
_start:
    mv s0, a0                       # save hart id  (a0) into a callee-saved reg
    mv s1, a1                       # save DTB ptr  (a1)
    la sp, __tx_boot_stack_top_load # stack top, LOW address (MMU still off)
    li t0, TX_RV64_MAX_BOOT_CPUS    # = 4
    bgeu s0, t0, .Ltx_bsp_stack_ready
    slli t1, s0, 17                 # hart_id * 128 KiB
    sub sp, sp, t1                  # carve this hart's stack slot
.Ltx_bsp_stack_ready:
```

`s0`/`s1` are callee-saved, so the firmware handoff registers survive everything
that follows. Each hart gets a 128 KiB (`1 << 17`) stack slice selected by hart
id — the doc requires this because the BSP's firmware hart id is *not assumed to
be zero* (`txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H1-…-1`).

### 2. Clear BSS

```asm
    la t0, __bss_start_load
    la t1, __bss_end_load
1:  bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b
```

A plain word-loop zeroing `.bss` at its low addresses. This also zeroes the
reserved page-table pages, so the next movement starts from clean tables.

### 3. Build the bootstrap Sv39 tables by hand

Three mappings go into the single bootstrap root page (`__bootstrap_root_load`).
Each is one store of a leaf PTE into a specific root slot. Recall a leaf PTE is
`(ppn << 10) | flags`, and `ppn = phys >> 12`, so `(phys >> 12) << 10` packs the
physical page number into PTE bit position.

**(a) Low identity leaf — slot 2 — the temporary bridge.**

```asm
2:  la s2, __bootstrap_root_load
    li t0, TX_RV64_QEMU_RAM_BASE    # 0x8000_0000
    srli t1, t0, 12
    slli t1, t1, 10
    ori  t1, t1, TX_RV64_PTE_IDENTITY   # V|R|W|X|A|D
    li   t2, TX_RV64_IDENTITY_ROOT_SLOT # 2
    slli t2, t2, 3
    add  t3, s2, t2
    sd   t1, 0(t3)                  # root[2] = 1 GiB identity leaf @ 0x8000_0000
```

Root slot 2 covers VA `[2 << 30, 3 << 30)` = `[0x8000_0000, 0xC000_0000)`, a 1 GiB
superpage mapping that VA range *to the same physical range*. This is the
identity bridge: it lets the trampoline keep executing at its current low PC for
the instant after the MMU turns on, before the jump to high addresses. It is
`RWX` because the trampoline code is in it.

**(b) High direct-map leaf — slot 258.**

```asm
    srli t1, t0, 12
    slli t1, t1, 10
    ori  t1, t1, TX_RV64_PTE_DIRECT     # V|R|W|G|A|D  (note: no X)
    li   t2, TX_RV64_DIRECT_MAP_ROOT_SLOT  # 258
    slli t2, t2, 3
    add  t3, s2, t2
    sd   t1, 0(t3)                  # root[258] = 1 GiB direct-map leaf
```

Root slot 258 is the first 1 GiB of the kernel direct map. With Sv39, slot 258
corresponds to VA `0xffff_ffc0_0000_0000` — exactly `DIRECT_MAP_BASE` from
Chapter 3. It maps that VA to physical `0x8000_0000`, global and non-executable
(the direct map is for data, not code).

**(c) High kernel-image alias — slot 510, via a sub-tree.**

The kernel image alias is not a single superpage; it is mapped at 2 MiB/4 KiB
granularity so text can be RX, rodata R, data RW. So slot 510 of the root points
at an L1 table, not a leaf:

```asm
    la   s3, __kernel_alias_l1_load
    srli t1, s3, 12
    slli t1, t1, 10
    ori  t1, t1, TX_RV64_PTE_V      # V only, no RWX → a branch (non-leaf) PTE
    li   t2, TX_RV64_KERNEL_ROOT_SLOT  # 510
    slli t2, t2, 3
    add  t3, s2, t2
    sd   t1, 0(t3)                  # root[510] → kernel-alias L1 table
```

A PTE with `V` set but `R/W/X` clear is a *branch* (it points at the next-level
table), as opposed to a leaf. Slot 510 lands at `KERNEL_VIRT_BASE`
(`0xffff_ffff_8020_0000`). Then a loop wires 16 L0 tables into L1 slots 1..16
(again as branch PTEs):

```asm
    la s4, __kernel_alias_l0_tables_load
    li t0, 0
    li t1, TX_RV64_KERNEL_ALIAS_L0_TABLES   # 16
3:  bgeu t0, t1, 4f
    slli t2, t0, 12                 # each L0 table is one 4 KiB page
    add  t3, s4, t2
    srli t4, t3, 12
    slli t4, t4, 10
    ori  t4, t4, TX_RV64_PTE_V      # branch
    li   t5, TX_RV64_KERNEL_L1_START_SLOT   # 1
    add  t5, t5, t0
    slli t5, t5, 3
    add  t6, s3, t5
    sd   t4, 0(t6)                  # L1[1 + i] → L0_tables[i]
    addi t0, t0, 1
    j 3b
```

Sixteen L0 tables × 2 MiB each = the 32 MiB `KERNEL_BOOTSTRAP_ALIAS_SIZE` window.
(The comment in `topology.rs:54` records *why* it's 32 MiB and not 16: the debug
kernel image grew past 16 MiB once the observe rings and the net subsystem
landed, and a kernel larger than this window faults the instant `satp` turns on
and loops in the trap vector. Under-mapping your own image is a real, debugged
bug here.)

Finally a loop fills the L0 leaves, one 4 KiB page per iteration, mapping each
page of the physical kernel image `[__kernel_start_load, __kernel_end_load)` into
the alias window:

```asm
4:  la s5, __kernel_start_load
    la s6, __kernel_end_load
    li s7, TX_RV64_PAGE_SIZE
    mv t0, s5
5:  bgeu t0, s6, 6f
    sub  t1, t0, s5                 # offset from image start
    srli t2, t1, 21                 # which L0 table (2 MiB chunks)
    slli t2, t2, 12
    add  t3, s4, t2
    srli t4, t1, 12                 # 4 KiB index within that table
    andi t4, t4, 0x1ff
    slli t4, t4, 3
    add  t3, t3, t4
    srli t5, t0, 12
    slli t5, t5, 10
    ori  t5, t5, TX_RV64_PTE_KERNEL_BOOT   # V|R|W|X|G|A|D
    sd   t5, 0(t3)
    add  t0, t0, s7
    j 5b
```

At this stage every page is mapped RWX (`TX_RV64_PTE_KERNEL_BOOT`). The fine
per-section permissions (text RX, rodata R, data RW) are refined later by the
Rust pmap (Chapter 8); the trampoline only needs the image *reachable*.

### 4. Turn on the MMU

```asm
6:  srli t0, s2, 12                 # bootstrap_root PPN
    li   t1, TX_RV64_SATP_SV39      # mode bits 0x8 << 60
    or   a0, t0, t1
    csrw satp, a0                   # paging ON
    sfence.vma                      # flush stale TLB
```

The instant after `csrw satp`, three VA windows are live: the low identity bridge
(so the *current* PC still resolves), the direct map at `0xffff_ffc0…`, and the
kernel alias at `0xffff_ffff_80…`. The `sfence.vma` makes the new translations
authoritative.

### 5. Rewrite `sp`/`gp` to high aliases and jump to Rust

```asm
    li t0, TX_RV64_KERNEL_VIRT_OFFSET     # 0xffffffff00000000
    la sp, __tx_boot_stack_top_load
    li t1, TX_RV64_MAX_BOOT_CPUS
    bgeu s0, t1, .Ltx_bsp_high_stack_ready
    slli t2, s0, 17
    sub  sp, sp, t2
.Ltx_bsp_high_stack_ready:
    add sp, sp, t0                  # sp: low → high alias of the SAME stack page
    .option push
    .option norelax
    la  gp, __global_pointer_load
    add gp, gp, t0                  # gp: low → high
    .option pop

    mv a0, s0                       # restore hart id
    mv a1, s1                       # restore DTB ptr — unchanged since entry
    la t1, __rust_entry_load
    add t1, t1, t0                  # rust_entry: low → high
    jr t1                           # enter Rust at its LINKED (high) address
```

This is the low→high stack-alias transition the doc describes
(`txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H1-…-1`): `sp` is rewritten to the *high alias
of the same physical stack storage* by adding `KERNEL_VIRT_OFFSET`. It does **not**
introduce per-thread kernel stacks — tasks remain stackless futures polled on the
current hart's stack (a theme that pays off enormously in Chapter 10). The `gp`
rewrite is wrapped in `.option norelax` so the assembler doesn't try to
GP-relative-optimize the very instruction that establishes `gp`.

The jump target is `__rust_entry_load + KERNEL_VIRT_OFFSET` — the high address of
the `rust_entry` symbol the *binary* exported (Chapter 1). The handoff registers
`a0`/`a1` are restored from `s0`/`s1`, satisfying H1 obligation 3: the raw CPU id
and firmware argument survive stack setup, BSS clear, and MMU activation
unchanged.

## What about the panic vector and early console?

The doc's H1 obligation list also includes a minimal trap vector (to catch a
panic) and an early console (to print it). On this board both are handled in the
Rust `entry` shell rather than in `_start`: `tx_hal::entry` calls
`P::install_minimal_trap_vector()` as its very first action (Chapter 6), and the
console is SBI-backed (`sbi_console_putchar`), so it needs no MMIO mapping to work
— `ConsoleIf::write_bytes` is usable the moment Rust runs. The smoke-boot contract
(`txdoc:HAL-THE-BOOT-SEQUENCE-PORTABLE-BOOT-CONTRACT-1`) requires only stack, BSS,
preserved registers, console, and reaching `rust_entry`; this board satisfies that
and the substrate-ready contract.

## What you should take away

- The kernel is linked high, loaded low; the linker exposes a `_load` symbol for
  every boot symbol so the MMU-off trampoline can address them.
- H1 builds three Sv39 windows by hand into BSS-reserved page-table pages — a low
  identity bridge, the direct map, and a fine-grained kernel-image alias — with
  no allocator, just stores.
- After `csrw satp`, `_start` rewrites `sp`/`gp`/PC from low to high aliases and
  jumps into Rust at its linked address. The DTB pointer rides through untouched.

Next: [Chapter 5 — Typestate as a boot-safety guard](ch05-bootstaticbag-typestate.md),
where Rust takes over and a non-`Copy`, non-`Clone` typestate machine makes it
*impossible* to use identity-era addresses after the bridge is torn down.
</content>
