OUTPUT_ARCH(loongarch64)
ENTRY(_start)

KERNEL_LOAD_BASE = 0x90000000;
KERNEL_LINK_BASE = 0x9000000090000000;
KERNEL_BOOT_PHYS_SIZE = 0x1000;
KERNEL_LINK_OFFSET = KERNEL_LINK_BASE - KERNEL_LOAD_BASE;
KERNEL_TRANSPORT_BASE = 0x98000000;

SECTIONS
{
    . = KERNEL_LOAD_BASE;
    .text.boot.phys : AT(KERNEL_LOAD_BASE) ALIGN(4K) {
        KEEP(*(.text.boot.phys))
    }
    ASSERT(SIZEOF(.text.boot.phys) <= KERNEL_BOOT_PHYS_SIZE,
           "2K1000 physical boot trampoline exceeds 4 KiB")

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
        . = ALIGN(16);
        _bss_end = .;
        __bss_end = .;
    }

    .bss.stack (NOLOAD) : AT(ADDR(.bss.stack) - KERNEL_LINK_OFFSET) ALIGN(4K) {
        __tx_boot_stack_bottom = .;
        . += 64K;
        . = ALIGN(16);
        __tx_boot_stack_top = .;
    }

    . = ALIGN(4K);
    __kernel_end = .;

    ASSERT((__kernel_end - KERNEL_LINK_OFFSET) <= KERNEL_TRANSPORT_BASE,
           "2K1000 kernel overlaps U-Boot TFTP transport address")

    /DISCARD/ : {
        *(.eh_frame .eh_frame.*)
        *(.note .note.*)
    }
}
