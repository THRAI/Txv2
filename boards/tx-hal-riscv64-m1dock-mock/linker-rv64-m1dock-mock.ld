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
    .text : AT(ADDR(.text) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        *(.text .text.*)
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        *(.srodata .srodata.*)
        *(.rodata .rodata.*)
    }

    .data : AT(ADDR(.data) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.sdata .sdata.*)
        *(.data .data.*)
    }
    __global_pointer_load = __global_pointer$ - KERNEL_VIRT_OFFSET;
    __rust_entry_load = rust_entry - KERNEL_VIRT_OFFSET;

    .bss (NOLOAD) : AT(ADDR(.bss) - KERNEL_VIRT_OFFSET) ALIGN(4K) {
        __bss_start = .;
        . = ALIGN(4K);
        __m1_boot_root = .;
        KEEP(*(.bss.boot.pagetable.m1dock_mock_root))
        . = ALIGN(4K);
        __m1_boot_mmio_l1 = .;
        KEEP(*(.bss.boot.pagetable.m1dock_mock_low_l1))
        . = ALIGN(4K);
        __m1_boot_uart_l0 = .;
        KEEP(*(.bss.boot.pagetable.m1dock_mock_uart_l0))
        . = ALIGN(4K);
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }
    __bss_start_load = __bss_start - KERNEL_VIRT_OFFSET;
    __bss_end_load = __bss_end - KERNEL_VIRT_OFFSET;
    __m1_boot_root_load = __m1_boot_root - KERNEL_VIRT_OFFSET;
    __m1_boot_mmio_l1_load = __m1_boot_mmio_l1 - KERNEL_VIRT_OFFSET;
    __m1_boot_uart_l0_load = __m1_boot_uart_l0 - KERNEL_VIRT_OFFSET;

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
