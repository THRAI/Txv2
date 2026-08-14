OUTPUT_ARCH(loongarch64)
ENTRY(_start)

KERNEL_LOAD_BASE = 0x98000000;
KERNEL_LINK_BASE = 0x9000000098000000;
KERNEL_BOOT_PHYS_SIZE = 0x1000;
KERNEL_LINK_OFFSET = KERNEL_LINK_BASE - KERNEL_LOAD_BASE;
INITRD_TRANSPORT_BASE = 0x98800000;
RAM1_END = 0xc0000000;
__kernel_only_mode = DEFINED(TX_KERNEL_ONLY) ? 1 : 0;
__kernel_payload_limit = __kernel_only_mode ? RAM1_END : INITRD_TRANSPORT_BASE;
KERNEL_RUNTIME_STACK_SIZE = __kernel_only_mode ? 256K : 128K;

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
        __tx_boot_stack_guard_bottom = .;
        . += 4K;
        __tx_boot_stack_guard_top = .;
        __tx_boot_stack_bottom = .;
        . += KERNEL_RUNTIME_STACK_SIZE;
        . = ALIGN(16);
        __tx_boot_stack_top = .;

        __tx_ap_boot_stack_guard_bottom = .;
        . += 4K;
        __tx_ap_boot_stack_guard_top = .;
        __tx_ap_boot_stack_bottom = .;
        . += KERNEL_RUNTIME_STACK_SIZE;
        . = ALIGN(16);
        __tx_ap_boot_stack_top = .;
    }

    ASSERT(__tx_boot_stack_guard_top - __tx_boot_stack_guard_bottom == 4K,
           "2K1000 BSP runtime stack guard must remain 4 KiB")
    ASSERT(__tx_boot_stack_top - __tx_boot_stack_bottom == KERNEL_RUNTIME_STACK_SIZE,
           "2K1000 BSP runtime stack does not match the image mode")
    ASSERT(__tx_ap_boot_stack_guard_top - __tx_ap_boot_stack_guard_bottom == 4K,
           "2K1000 AP runtime stack guard must remain 4 KiB")
    ASSERT(__tx_ap_boot_stack_top - __tx_ap_boot_stack_bottom == KERNEL_RUNTIME_STACK_SIZE,
           "2K1000 AP runtime stack does not match the image mode")

    . = ALIGN(4K);
    __kernel_end = .;
    __kernel_end_phys = __kernel_end - KERNEL_LINK_OFFSET;

    ASSERT(__kernel_only_mode || __kernel_end_phys <= INITRD_TRANSPORT_BASE,
           "2K1000 kernel overlaps validated initrd transport address")
    ASSERT(!__kernel_only_mode || __kernel_end_phys <= RAM1_END,
           "2K1000 kernel-only image exceeds RAM1")

    /DISCARD/ : {
        *(.eh_frame .eh_frame.*)
        *(.note .note.*)
    }
}
