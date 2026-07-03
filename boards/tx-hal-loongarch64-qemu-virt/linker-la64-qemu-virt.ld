OUTPUT_ARCH(loongarch64)
ENTRY(_start)

/* Unified load base for QEMU virt AND the LS2K1000 board: both have
 * RAM at phys 0x9000_0000 (QEMU: the >256M high region, always
 * present at our -m 1152M; 2K1000: the high DDR window). The 2K1000
 * U-Boot flow loads the uImage payload at 0x9000000090000000 (the
 * cached-DMW alias of this base — NPUcore's proven -a/-e address),
 * so one binary serves both worlds, mirroring the RV64 0x80200000
 * arrangement. */
KERNEL_LOAD_BASE = 0x90000000;
KERNEL_LINK_BASE = 0x9000000090000000;
KERNEL_BOOT_PHYS_SIZE = 0x1000;
KERNEL_LINK_OFFSET = KERNEL_LINK_BASE - KERNEL_LOAD_BASE;

SECTIONS
{
    . = KERNEL_LOAD_BASE;
    .text.boot.phys : AT(KERNEL_LOAD_BASE) ALIGN(4K) {
        KEEP(*(.text.boot.phys))
    }

    . = KERNEL_LINK_BASE;
    __kernel_start = .;

    . = KERNEL_LINK_BASE + KERNEL_BOOT_PHYS_SIZE;
    .text : AT(ADDR(.text) - KERNEL_LINK_OFFSET) ALIGN(4K) {
        KEEP(*(.text.boot.high))
        *(.text .text.*)
    }
    __text_end = .;

    .rodata : AT(ADDR(.rodata) - KERNEL_LINK_OFFSET) ALIGN(4K) {
        *(.srodata .srodata.*)
        *(.rodata .rodata.*)
    }
    __rodata_end = .;

    .data : AT(ADDR(.data) - KERNEL_LINK_OFFSET) ALIGN(4K) {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.sdata .sdata.*)
        *(.data .data.*)
    }
    __data_end = .;

    .bss (NOLOAD) : AT(ADDR(.bss) - KERNEL_LINK_OFFSET) ALIGN(4K) {
        _bss_start = .;
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        _bss_end = .;
        __bss_end = .;
    }

    .bss.stack (NOLOAD) : AT(ADDR(.bss.stack) - KERNEL_LINK_OFFSET) ALIGN(4K) {
        __tx_boot_stack_bottom = .;
        . += 512K;
        . = ALIGN(16);
        __tx_boot_stack_top = .;
    }

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame.*)
        *(.note .note.*)
    }
}
