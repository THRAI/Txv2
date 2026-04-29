OUTPUT_ARCH(riscv)
ENTRY(_start)

KERNEL_LOAD_BASE = 0x80200000;
KERNEL_VIRT_BASE = 0xffffffff80200000;
KERNEL_VIRT_OFFSET = KERNEL_VIRT_BASE - KERNEL_LOAD_BASE;

SECTIONS
{
    . = KERNEL_LOAD_BASE;

    .text.trampoline : ALIGN(4K) {
        KEEP(*(.text.trampoline))
    }

    . = KERNEL_VIRT_BASE;
    __kernel_start = .;
    __kernel_start_load = KERNEL_LOAD_BASE;

    . += SIZEOF(.text.trampoline);
    . = ALIGN(4K);
    __text_start = .;
    .text : AT(ADDR(.text) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        *(.text .text.*)
    }
    __text_end = .;
    __text_start_load = LOADADDR(.text);
    __text_end_load = LOADADDR(.text) + SIZEOF(.text);

    __rodata_start = .;
    .rodata : AT(ADDR(.rodata) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        *(.srodata .srodata.*)
        *(.rodata .rodata.*)
    }
    __rodata_end = .;
    __rodata_start_load = LOADADDR(.rodata);
    __rodata_end_load = LOADADDR(.rodata) + SIZEOF(.rodata);

    __data_start = .;
    .data : AT(ADDR(.data) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.sdata .sdata.*)
        *(.data .data.*)
    }
    __data_end = .;
    __data_start_load = LOADADDR(.data);
    __data_end_load = LOADADDR(.data) + SIZEOF(.data);
    __global_pointer_load = __global_pointer$ - KERNEL_VIRT_OFFSET;
    __rust_entry_load = rust_entry - KERNEL_VIRT_OFFSET;

    .bss (NOLOAD) : AT(ADDR(.bss) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        __bss_start = .;
        . = ALIGN(4K);
        __bootstrap_root = .;
        KEEP(*(.bss.boot.pagetable.bootstrap_root))
        . = ALIGN(4K);
        __kernel_alias_l1 = .;
        KEEP(*(.bss.boot.pagetable.kernel_alias_l1))
        . = ALIGN(4K);
        __kernel_alias_l0_tables = .;
        KEEP(*(.bss.boot.pagetable.kernel_alias_l0_tables))
        . = ALIGN(4K);
        __pt_node_pool = .;
        KEEP(*(.bss.boot.pagetable.pt_node_pool))
        . = ALIGN(4K);
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }
    __bss_start_load = __bss_start - KERNEL_VIRT_OFFSET;
    __bss_end_load = __bss_end - KERNEL_VIRT_OFFSET;
    __bootstrap_root_load = __bootstrap_root - KERNEL_VIRT_OFFSET;
    __kernel_alias_l1_load = __kernel_alias_l1 - KERNEL_VIRT_OFFSET;
    __kernel_alias_l0_tables_load = __kernel_alias_l0_tables - KERNEL_VIRT_OFFSET;
    __pt_node_pool_load = __pt_node_pool - KERNEL_VIRT_OFFSET;

    .bss.stack (NOLOAD) : AT(ADDR(.bss.stack) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        __tx_boot_stack_bottom = .;
        . += 64K;
        . = ALIGN(16);
        __tx_boot_stack_top = .;
    }
    __tx_boot_stack_bottom_load = __tx_boot_stack_bottom - KERNEL_VIRT_OFFSET;
    __tx_boot_stack_top_load = __tx_boot_stack_top - KERNEL_VIRT_OFFSET;

    . = ALIGN(4K);
    __kernel_end = .;
    __kernel_end_load = __kernel_end - KERNEL_VIRT_OFFSET;

    /DISCARD/ : {
        *(.eh_frame .eh_frame.*)
        *(.note .note.*)
    }
}
