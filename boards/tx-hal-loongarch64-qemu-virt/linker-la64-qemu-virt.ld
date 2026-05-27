OUTPUT_ARCH(loongarch64)
ENTRY(_start)

KERNEL_LOAD_BASE = 0x00200000;
KERNEL_LINK_BASE = 0x9000000000200000;
KERNEL_LINK_OFFSET = KERNEL_LINK_BASE - KERNEL_LOAD_BASE;

SECTIONS
{
    . = KERNEL_LINK_BASE;
    __kernel_start = .;

    .text : AT(ADDR(.text) - KERNEL_LINK_OFFSET) ALIGN(4K) {
        KEEP(*(.text.boot))
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
