OUTPUT_ARCH(loongarch64)
ENTRY(_start)

KERNEL_LOAD_BASE = 0x00200000;

SECTIONS
{
    . = KERNEL_LOAD_BASE;
    __kernel_start = .;

    .text : ALIGN(4K) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
    __text_end = .;

    .rodata : ALIGN(4K) {
        *(.srodata .srodata.*)
        *(.rodata .rodata.*)
    }
    __rodata_end = .;

    .data : ALIGN(4K) {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.sdata .sdata.*)
        *(.data .data.*)
    }
    __data_end = .;

    .bss (NOLOAD) : ALIGN(4K) {
        _bss_start = .;
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        _bss_end = .;
        __bss_end = .;
    }

    .bss.stack (NOLOAD) : ALIGN(4K) {
        __tx_boot_stack_bottom = .;
        . += 256K;
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
