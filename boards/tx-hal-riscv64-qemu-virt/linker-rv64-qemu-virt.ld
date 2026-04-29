OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;

SECTIONS
{
    . = BASE_ADDRESS;
    __kernel_start = .;
    __text_start = .;

    .text.boot : ALIGN(4K) {
        KEEP(*(.text.boot))
    }

    .text : ALIGN(4K) {
        *(.text .text.*)
    }
    __text_end = .;

    __rodata_start = .;
    .rodata : ALIGN(4K) {
        *(.srodata .srodata.*)
        *(.rodata .rodata.*)
    }
    __rodata_end = .;

    __data_start = .;
    .data : ALIGN(4K) {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.sdata .sdata.*)
        *(.data .data.*)
    }
    __data_end = .;

    .bss (NOLOAD) : ALIGN(4K) {
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    .bss.stack (NOLOAD) : ALIGN(4K) {
        __tx_boot_stack_bottom = .;
        . += 64K;
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
